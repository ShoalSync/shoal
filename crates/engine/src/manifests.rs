//! A folder's manifests and chunk index, kept on disk (ADR 0035): a folder of terabytes has
//! tens of millions of chunks, too many to hold in memory. Reads go to the database, through a
//! small cache; what changed since the committer last confirmed a save is kept here, so it reads
//! back before it is committed.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tidefell_proto::{ChunkHash, ContentHash, Manifest};
use tracing::warn;

use crate::db::{chunk_rows, Reader};
use crate::tracked::{encode, Changed};

/// Manifests read from the database kept for the next read.
const CACHED: usize = 256;

/// A change not yet known to be committed, and the save that carried it (0 before one did).
type Pending<T> = (T, u64);

#[derive(Debug, Default)]
pub(crate) struct Manifests {
    /// None until the folder's database exists; everything is pending meanwhile.
    reader: Option<Reader>,
    /// Manifests added (`Some`) or removed since their save was confirmed.
    overlay: HashMap<ContentHash, Pending<Option<Arc<Manifest>>>>,
    /// Chunk index rows added (`true`) or removed likewise.
    rows: HashMap<[u8; 64], Pending<bool>>,
    cache: Mutex<Cache>,
}

#[derive(Debug, Default)]
struct Cache {
    manifests: HashMap<ContentHash, Arc<Manifest>>,
    order: VecDeque<ContentHash>,
}

impl Cache {
    fn get(&self, content: &ContentHash) -> Option<Arc<Manifest>> {
        self.manifests.get(content).cloned()
    }

    fn put(&mut self, manifest: Arc<Manifest>) {
        if self
            .manifests
            .insert(manifest.content, manifest.clone())
            .is_none()
        {
            self.order.push_back(manifest.content);
        }
        while self.order.len() > CACHED {
            if let Some(old) = self.order.pop_front() {
                self.manifests.remove(&old);
            }
        }
    }

    fn forget(&mut self, content: &ContentHash) {
        if self.manifests.remove(content).is_some() {
            self.order.retain(|c| c != content);
        }
    }
}

impl Manifests {
    /// The folder's database, once it exists.
    pub(crate) fn reader(&self) -> Option<&Reader> {
        self.reader.as_ref()
    }

    /// Reads from `reader` from now on.
    pub(crate) fn set_reader(&mut self, reader: Reader) {
        self.reader = Some(reader);
    }

    /// Stops reading the database, so that it closes once its committer has: it can then be
    /// opened again at once, as after a restart. Nothing is read from it afterwards.
    pub(crate) fn close(&mut self) {
        self.reader = None;
    }

    /// The manifest of `content`, if the folder has one.
    pub(crate) fn get(&self, content: &ContentHash) -> Option<Arc<Manifest>> {
        if let Some((manifest, _)) = self.overlay.get(content) {
            return manifest.clone();
        }
        let mut cache = self.cache.lock().unwrap();
        if let Some(manifest) = cache.get(content) {
            return Some(manifest);
        }
        let manifest = Arc::new(self.read(|r| r.manifest(content))??);
        cache.put(manifest.clone());
        Some(manifest)
    }

    /// True if the folder has a manifest of `content`.
    pub(crate) fn contains_key(&self, content: &ContentHash) -> bool {
        self.get(content).is_some()
    }

    fn read<T>(&self, read: impl FnOnce(&Reader) -> crate::Result<T>) -> Option<T> {
        let reader = self.reader.as_ref()?;
        match read(reader) {
            Ok(value) => Some(value),
            Err(e) => {
                warn!(error = %e, "reading the folder's manifests failed");
                None
            }
        }
    }

    /// Records `manifest`, and its chunks in the index.
    pub(crate) fn insert(&mut self, manifest: Manifest) {
        for row in chunk_rows(&manifest) {
            self.rows.insert(row, (true, 0));
        }
        self.cache.lock().unwrap().forget(&manifest.content);
        self.overlay
            .insert(manifest.content, (Some(Arc::new(manifest)), 0));
    }

    /// Forgets the manifest of `content`, and returns its chunks no other content has.
    pub(crate) fn remove(&mut self, content: &ContentHash) -> Vec<ChunkHash> {
        let Some(manifest) = self.get(content) else {
            return Vec::new();
        };
        for row in chunk_rows(&manifest) {
            self.rows.insert(row, (false, 0));
        }
        self.cache.lock().unwrap().forget(content);
        self.overlay.insert(*content, (None, 0));
        let mut orphaned: Vec<ChunkHash> = manifest
            .chunks
            .iter()
            .map(|c| c.hash)
            .filter(|hash| !self.is_referenced(hash))
            .collect();
        orphaned.sort_unstable();
        orphaned.dedup();
        orphaned
    }

    /// The contents holding chunk `hash`.
    pub(crate) fn contents_with(&self, hash: &ChunkHash) -> Vec<ContentHash> {
        let mut contents = self.read(|r| r.contents_with(hash)).unwrap_or_default();
        let row = |content: &ContentHash| {
            let mut row = [0u8; 64];
            row[..32].copy_from_slice(&hash.0);
            row[32..].copy_from_slice(&content.0);
            row
        };
        contents.retain(|c| self.rows.get(&row(c)).is_none_or(|(add, _)| *add));
        for (key, (add, _)) in &self.rows {
            if *add && key[..32] == hash.0 {
                let mut content = [0u8; 32];
                content.copy_from_slice(&key[32..]);
                let content = ContentHash(content);
                if !contents.contains(&content) {
                    contents.push(content);
                }
            }
        }
        contents
    }

    /// True if some content holds chunk `hash`.
    pub(crate) fn is_referenced(&self, hash: &ChunkHash) -> bool {
        !self.contents_with(hash).is_empty()
    }

    /// Every content the folder has a manifest of.
    pub(crate) fn contents(&self) -> Vec<ContentHash> {
        let mut contents = self.read(Reader::contents).unwrap_or_default();
        contents.retain(|c| self.overlay.get(c).is_none_or(|(m, _)| m.is_some()));
        for (content, (manifest, _)) in &self.overlay {
            if manifest.is_some() && !contents.contains(content) {
                contents.push(*content);
            }
        }
        contents
    }

    /// The changes not yet handed to a save, as manifest records and chunk index rows, marked
    /// as carried by save `generation`.
    pub(crate) fn take_changed(
        &mut self,
        generation: u64,
    ) -> (Vec<Changed>, Vec<([u8; 64], bool)>) {
        let mut records = Vec::new();
        for (content, (manifest, carried)) in &mut self.overlay {
            if *carried == 0 {
                *carried = generation;
                records.push((encode(content), manifest.as_deref().map(encode)));
            }
        }
        let mut rows = Vec::new();
        for (row, (add, carried)) in &mut self.rows {
            if *carried == 0 {
                *carried = generation;
                rows.push((*row, *add));
            }
        }
        (records, rows)
    }

    /// Everything to write into a new database: what is here, as nothing is committed yet.
    pub(crate) fn all(&self) -> (Vec<Changed>, Vec<([u8; 64], bool)>) {
        let records = self
            .overlay
            .iter()
            .filter_map(|(c, (m, _))| m.as_deref().map(|m| (encode(c), Some(encode(m)))))
            .collect();
        let rows = self
            .rows
            .iter()
            .filter(|(_, (add, _))| *add)
            .map(|(row, _)| (*row, true))
            .collect();
        (records, rows)
    }

    /// Forgets the changes saves up to `generation` carried, which the database now has.
    pub(crate) fn committed(&mut self, generation: u64) {
        self.overlay
            .retain(|_, (_, carried)| *carried == 0 || *carried > generation);
        self.rows
            .retain(|_, (_, carried)| *carried == 0 || *carried > generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidefell_proto::ChunkRef;

    fn manifest(content: u8, chunks: &[u8]) -> Manifest {
        Manifest {
            content: ContentHash([content; 32]),
            chunks: chunks
                .iter()
                .map(|&c| ChunkRef {
                    hash: ChunkHash([c; 32]),
                    len: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn manifests_read_back_before_and_after_they_are_saved() {
        let mut manifests = Manifests::default();
        manifests.insert(manifest(1, &[10, 11]));
        manifests.insert(manifest(2, &[11, 12]));
        assert_eq!(
            manifests.get(&ContentHash([1; 32])).unwrap().chunks.len(),
            2
        );
        let mut with = manifests.contents_with(&ChunkHash([11; 32]));
        with.sort();
        assert_eq!(with, [ContentHash([1; 32]), ContentHash([2; 32])]);
        // Chunk 11 is still in content 2; chunk 10 was only in content 1.
        assert_eq!(
            manifests.remove(&ContentHash([1; 32])),
            [ChunkHash([10; 32])]
        );
        assert!(!manifests.is_referenced(&ChunkHash([10; 32])));
        assert_eq!(manifests.contents(), [ContentHash([2; 32])]);
        let (records, rows) = manifests.take_changed(1);
        assert_eq!(records.len(), 2);
        assert_eq!(rows.len(), 4);
        assert!(manifests.take_changed(2).0.is_empty(), "handed over once");
    }
}
