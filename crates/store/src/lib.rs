//! Tidefell `store` crate: block store, availability bitmaps, eviction, on-disk layout.
//!
//! # On-disk layout (format 1)
//!
//! ```text
//! <root>/FORMAT                  "tidefell-store 1\n"
//! <root>/chunks/ab/cdef…         chunk bytes, named by lowercase hex BLAKE3 hash,
//!                                split after the first two hex digits
//! <root>/tmp/                    in-progress writes, renamed into place when complete
//! ```
//!
//! Chunks are written to `tmp/` and renamed, so a crash never leaves a partial chunk under
//! `chunks/`. Every read is verified against the chunk's hash.

mod availability;

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tidefell_chunker::hash_chunk;
use tidefell_proto::{ChunkHash, Manifest};

pub use availability::Availability;

/// Contents of the `FORMAT` file for the layout this crate writes.
pub const FORMAT: &str = "tidefell-store 1\n";

/// Block store errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem operation failed.
    #[error("store I/O error: {0}")]
    Io(#[from] io::Error),
    /// Data offered for a chunk does not hash to that chunk.
    #[error("data does not match chunk {expected}")]
    HashMismatch {
        /// The chunk the data was offered for.
        expected: ChunkHash,
    },
    /// A stored chunk no longer matches its hash. It has been removed.
    #[error("chunk {0} was corrupt and has been removed")]
    Corrupt(ChunkHash),
    /// The directory holds a store in a format this version cannot read.
    #[error("unsupported store format {0:?}")]
    UnsupportedFormat(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy)]
struct Meta {
    len: u64,
    /// Logical access time; larger is more recent.
    used: u64,
}

/// A content-addressed chunk store in one directory.
#[derive(Debug)]
pub struct BlockStore {
    root: PathBuf,
    chunks: HashMap<ChunkHash, Meta>,
    /// Access time to chunk, for least-recently-used eviction.
    lru: BTreeMap<u64, ChunkHash>,
    clock: u64,
    total_bytes: u64,
}

impl BlockStore {
    /// Opens the store at `root`, creating it if the directory is empty or missing.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let format_path = root.join("FORMAT");
        match fs::read_to_string(&format_path) {
            Ok(format) if format == FORMAT => {}
            Ok(other) => return Err(Error::UnsupportedFormat(other)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if fs::read_dir(&root)?.next().is_some() {
                    return Err(Error::UnsupportedFormat(String::new()));
                }
                fs::write(&format_path, FORMAT)?;
            }
            Err(e) => return Err(e.into()),
        }
        let tmp = root.join("tmp");
        if tmp.exists() {
            fs::remove_dir_all(&tmp)?;
        }
        fs::create_dir_all(&tmp)?;
        fs::create_dir_all(root.join("chunks"))?;

        let mut store = Self {
            root,
            chunks: HashMap::new(),
            lru: BTreeMap::new(),
            clock: 0,
            total_bytes: 0,
        };
        store.scan()?;
        Ok(store)
    }

    /// Loads the index of stored chunks, oldest modification time first.
    fn scan(&mut self) -> Result<()> {
        let mut found = Vec::new();
        for shard in fs::read_dir(self.root.join("chunks"))? {
            let shard = shard?;
            let prefix = shard.file_name().to_string_lossy().into_owned();
            for file in fs::read_dir(shard.path())? {
                let file = file?;
                let name = format!("{prefix}{}", file.file_name().to_string_lossy());
                let Ok(hash) = name.parse::<ChunkHash>() else {
                    continue;
                };
                let meta = file.metadata()?;
                found.push((meta.modified().ok(), hash, meta.len()));
            }
        }
        found.sort();
        for (_, hash, len) in found {
            self.record(hash, len);
        }
        Ok(())
    }

    fn record(&mut self, hash: ChunkHash, len: u64) {
        self.clock += 1;
        if let Some(old) = self.chunks.insert(
            hash,
            Meta {
                len,
                used: self.clock,
            },
        ) {
            self.lru.remove(&old.used);
            self.total_bytes -= old.len;
        }
        self.lru.insert(self.clock, hash);
        self.total_bytes += len;
    }

    fn touch(&mut self, hash: &ChunkHash) {
        if let Some(meta) = self.chunks.get_mut(hash) {
            self.lru.remove(&meta.used);
            self.clock += 1;
            meta.used = self.clock;
            self.lru.insert(self.clock, *hash);
        }
    }

    fn path(&self, hash: &ChunkHash) -> PathBuf {
        let hex = hash.to_string();
        self.root.join("chunks").join(&hex[..2]).join(&hex[2..])
    }

    /// The store's root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Stores `data` and returns its hash. Storing an existing chunk is a no-op.
    pub fn put(&mut self, data: &[u8]) -> Result<ChunkHash> {
        let hash = hash_chunk(data);
        self.write(hash, data)?;
        Ok(hash)
    }

    /// Stores `data` as chunk `expected`, rejecting data that does not match.
    pub fn put_verified(&mut self, expected: ChunkHash, data: &[u8]) -> Result<()> {
        if hash_chunk(data) != expected {
            return Err(Error::HashMismatch { expected });
        }
        self.write(expected, data)
    }

    fn write(&mut self, hash: ChunkHash, data: &[u8]) -> Result<()> {
        if self.chunks.contains_key(&hash) {
            self.touch(&hash);
            return Ok(());
        }
        let dest = self.path(&hash);
        fs::create_dir_all(dest.parent().expect("chunk paths have a shard directory"))?;
        let tmp = self.root.join("tmp").join(hash.to_string());
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(data)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, &dest)?;
        self.record(hash, data.len() as u64);
        Ok(())
    }

    /// Reads a chunk, verifying its hash. Returns `None` if the chunk is not stored.
    pub fn get(&mut self, hash: &ChunkHash) -> Result<Option<Vec<u8>>> {
        if !self.chunks.contains_key(hash) {
            return Ok(None);
        }
        let data = match fs::read(self.path(hash)) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                self.forget(hash);
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        };
        if hash_chunk(&data) != *hash {
            self.remove(hash)?;
            return Err(Error::Corrupt(*hash));
        }
        self.touch(hash);
        Ok(Some(data))
    }

    /// True if the chunk is stored.
    pub fn contains(&self, hash: &ChunkHash) -> bool {
        self.chunks.contains_key(hash)
    }

    /// Removes a chunk. Returns false if it was not stored.
    pub fn remove(&mut self, hash: &ChunkHash) -> Result<bool> {
        if !self.forget(hash) {
            return Ok(false);
        }
        match fs::remove_file(self.path(hash)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
            Err(e) => Err(e.into()),
        }
    }

    fn forget(&mut self, hash: &ChunkHash) -> bool {
        match self.chunks.remove(hash) {
            Some(meta) => {
                self.lru.remove(&meta.used);
                self.total_bytes -= meta.len;
                true
            }
            None => false,
        }
    }

    /// The hashes of every stored chunk, in no particular order.
    pub fn hashes(&self) -> impl Iterator<Item = &ChunkHash> {
        self.chunks.keys()
    }

    /// Number of chunks stored.
    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    /// True if no chunks are stored.
    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    /// Total bytes of chunk data stored.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Which of a manifest's chunks are stored.
    pub fn availability(&self, manifest: &Manifest) -> Availability {
        Availability::from_fn(manifest.chunks.len(), |i| {
            self.contains(&manifest.chunks[i].hash)
        })
    }

    /// Removes least-recently-used chunks until at most `budget` bytes remain, skipping any
    /// chunk for which `pinned` returns true. Returns the evicted chunks, oldest first.
    ///
    /// If pinned chunks alone exceed the budget, everything unpinned is evicted.
    pub fn evict_to(
        &mut self,
        budget: u64,
        pinned: impl Fn(&ChunkHash) -> bool,
    ) -> Result<Vec<ChunkHash>> {
        let mut candidates = Vec::new();
        let mut projected = self.total_bytes;
        for hash in self.lru.values() {
            if projected <= budget {
                break;
            }
            if !pinned(hash) {
                projected -= self.chunks[hash].len;
                candidates.push(*hash);
            }
        }
        for hash in &candidates {
            self.remove(hash)?;
        }
        Ok(candidates)
    }
}

#[cfg(test)]
mod tests {
    use tidefell_proto::{ChunkRef, ContentHash};

    use super::*;

    fn store() -> (tempfile::TempDir, BlockStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = BlockStore::open(dir.path().join("blocks")).unwrap();
        (dir, store)
    }

    #[test]
    fn put_get_remove() {
        let (_dir, mut store) = store();
        let hash = store.put(b"hello").unwrap();
        assert_eq!(hash, hash_chunk(b"hello"));
        assert_eq!(store.put(b"hello").unwrap(), hash);
        assert_eq!(store.len(), 1);
        assert_eq!(store.total_bytes(), 5);
        assert_eq!(store.get(&hash).unwrap().unwrap(), b"hello");
        assert!(store.remove(&hash).unwrap());
        assert!(!store.remove(&hash).unwrap());
        assert_eq!(store.get(&hash).unwrap(), None);
        assert_eq!(store.total_bytes(), 0);
    }

    #[test]
    fn rejects_mismatched_data() {
        let (_dir, mut store) = store();
        let err = store
            .put_verified(hash_chunk(b"expected"), b"actual")
            .unwrap_err();
        assert!(matches!(err, Error::HashMismatch { .. }));
        assert!(store.is_empty());
    }

    #[test]
    fn reopen_restores_index() {
        let dir = tempfile::tempdir().unwrap();
        let hash = {
            let mut store = BlockStore::open(dir.path()).unwrap();
            store.put(b"persisted").unwrap()
        };
        let mut store = BlockStore::open(dir.path()).unwrap();
        assert!(store.contains(&hash));
        assert_eq!(store.total_bytes(), 9);
        assert_eq!(store.get(&hash).unwrap().unwrap(), b"persisted");
    }

    #[test]
    fn corrupt_chunk_is_detected_and_removed() {
        let (_dir, mut store) = store();
        let hash = store.put(b"original").unwrap();
        fs::write(store.path(&hash), b"tampered").unwrap();
        assert!(matches!(store.get(&hash), Err(Error::Corrupt(_))));
        assert!(!store.contains(&hash));
    }

    #[test]
    fn refuses_unknown_format_and_foreign_directories() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("FORMAT"), "tidefell-store 99\n").unwrap();
        assert!(matches!(
            BlockStore::open(dir.path()),
            Err(Error::UnsupportedFormat(_))
        ));

        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join("unrelated.txt"), "x").unwrap();
        assert!(matches!(
            BlockStore::open(other.path()),
            Err(Error::UnsupportedFormat(_))
        ));
    }

    #[test]
    fn eviction_is_lru_and_respects_pins() {
        let (_dir, mut store) = store();
        let a = store.put(&[1; 100]).unwrap();
        let b = store.put(&[2; 100]).unwrap();
        let c = store.put(&[3; 100]).unwrap();
        let d = store.put(&[4; 100]).unwrap();
        store.get(&a).unwrap(); // a is now most recently used

        let evicted = store.evict_to(200, |h| *h == b).unwrap();
        assert_eq!(evicted, vec![c, d]);
        assert!(store.contains(&a) && store.contains(&b));
        assert_eq!(store.total_bytes(), 200);

        let evicted = store.evict_to(0, |h| *h == b).unwrap();
        assert_eq!(evicted, vec![a]);
        assert_eq!(store.total_bytes(), 100);
    }

    #[test]
    fn availability_reflects_stored_chunks() {
        let (_dir, mut store) = store();
        let a = store.put(b"a").unwrap();
        let manifest = Manifest {
            content: ContentHash([0; 32]),
            chunks: vec![
                ChunkRef { hash: a, len: 1 },
                ChunkRef {
                    hash: hash_chunk(b"missing"),
                    len: 7,
                },
                ChunkRef { hash: a, len: 1 },
            ],
        };
        let avail = store.availability(&manifest);
        assert_eq!(avail.len(), 3);
        assert!(avail.get(0) && !avail.get(1) && avail.get(2));
        assert_eq!(avail.count(), 2);
        assert!(!avail.is_complete());
    }
}
