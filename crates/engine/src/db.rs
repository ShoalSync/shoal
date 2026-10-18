//! Each folder's metadata in a database of its own (ADR 0034), so a change writes only the
//! records it touches.
//!
//! Versions are kept by the sequence this device stored them under, so storing appends. A
//! version superseded later is listed as dropped, in a table that is also appended to, and the
//! dropped versions are removed in one sorted pass once there are as many as live ones; the
//! file is then compacted, as redb keeps freed pages otherwise. The engine's own records, such
//! as files on disk and manifests, are one table each, by key.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tidefell_meta::{Header, Journal, Recycled, Retention, Wire};
use tidefell_proto::sync::EntryVersion;
use tidefell_proto::sync2::SealedVersion;
use tidefell_proto::{ChunkHash, ContentHash, DeviceId, FolderId, Manifest, Signed};

use crate::tracked::Changed;
use crate::{Error, Result};

/// The layout of the tables below; a database of another format is not read.
const FORMAT: u32 = 1;

const HEADER: TableDefinition<&str, &[u8]> = TableDefinition::new("header");
const VERSIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("versions");
const DROPPED: TableDefinition<u64, &[u8]> = TableDefinition::new("dropped");
const RECYCLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("recycle");
/// Which contents hold each chunk (ADR 0035): the chunk's hash then the content's, 64 bytes,
/// so the contents with a chunk are one range.
const CHUNKS: TableDefinition<&[u8], ()> = TableDefinition::new("chunks");
/// The manifests table, read one record at a time rather than at open (ADR 0035).
const MANIFESTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("manifests");

/// The engine's records, one table each, by name.
pub(crate) const MAPS: [&str; 17] = [
    "manifests",
    "local",
    "progress",
    "used",
    "copies",
    "copy_names",
    "dropped_entries",
    "set_aside",
    "kept",
    "history",
    "pins",
    // Added after the first format: a database from before has no such table, and reads as
    // having none of these (see `FolderDb::open`).
    "replaced",
    // An encrypted folder's (ADR 0023): the folder keys held, the index's items besides
    // versions, and which chunk each chunk ID is.
    "keys",
    "items",
    "chunk_ids",
    "sealed_chunks",
    // A stored folder's (ADR 0023): when each superseded sealed version was superseded.
    "superseded",
];

/// In the header table, present in an encrypted folder's database: its versions rows hold
/// sealed versions (ADR 0023).
const ENCRYPTED: &str = "encrypted";

/// Dropped versions are removed once there are this many at least, and as many as live ones.
const CLEAN_AT_LEAST: u64 = 4096;

/// The header as stored.
#[derive(Debug, Serialize, Deserialize)]
struct HeaderRecord {
    format: u32,
    folder: FolderId,
    device: DeviceId,
    epoch: u64,
    sequence: u64,
    retention: Retention,
}

/// A table's records, encoded keys and values.
pub(crate) type Records = Vec<(Vec<u8>, Vec<u8>)>;

/// A folder's rebuild (ADR 0034): when it began and why, and when it finished, if it has. Kept
/// in the database, so a rebuild cut short carries on when the engine starts again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Rebuild {
    pub(crate) started_ms: i64,
    pub(crate) reason: String,
    /// The recycle bin, history and files set aside were restored from their sidecars, which
    /// waits for the folder's root to be there.
    pub(crate) restored: bool,
    pub(crate) finished_ms: Option<i64>,
}

/// What to write in one transaction.
pub(crate) struct Changes {
    pub(crate) header: Header,
    pub(crate) journal: Journal,
    /// By table name, from [`MAPS`].
    pub(crate) maps: Vec<(&'static str, Vec<Changed>)>,
    /// Wait for the disk before returning: for changes peers may be told about, or that the
    /// engine cannot make again, such as signed versions and index progress.
    pub(crate) durable: bool,
    /// The folder's last rebuild, if it had one.
    pub(crate) rebuild: Option<Rebuild>,
    /// What the changes did, for the activity log (ADR 0030), written once they are committed.
    pub(crate) activity: Vec<tidefell_meta::Change>,
    /// When they were applied.
    pub(crate) at_ms: i64,
    /// Rows of the chunk index to add (`true`) or remove: a chunk's hash, then a content's.
    pub(crate) chunks: Vec<([u8; 64], bool)>,
    /// Which save this is, counted by the folder, so it knows once the changes are committed.
    pub(crate) generation: u64,
}

/// What a database holds.
pub(crate) struct Loaded {
    pub(crate) header: Header,
    /// The folder's last rebuild, if it had one.
    pub(crate) rebuild: Option<Rebuild>,
    /// The versions still kept, in sequence order, each with what was decrypted from it if it
    /// is sealed.
    pub(crate) versions: Vec<(u64, Wire, Option<EntryVersion>)>,
    pub(crate) recycled: Vec<Recycled>,
    /// By table name, encoded keys and values.
    pub(crate) maps: BTreeMap<&'static str, Records>,
}

/// Databases up to this size are checked when opened (ADR 0034), which takes a few seconds at
/// most. A larger one, which can take minutes, is checked soon after instead, so a large folder
/// is not kept waiting to start (ADR 0035).
const CHECKED_AT_OPEN: u64 = 1 << 30;

/// One folder's database.
pub(crate) struct FolderDb {
    /// Shared with [`Reader`]s; only the integrity check and compaction need it alone.
    db: Arc<RwLock<Database>>,
    path: PathBuf,
    /// Versions kept.
    live: u64,
    /// Versions dropped but not yet removed.
    dropped: u64,
    /// The next key of the dropped table.
    next_drop: u64,
    /// Whether it was checked when opened.
    checked: bool,
    /// An encrypted folder's database, whose rows hold sealed versions.
    encrypted: bool,
}

impl std::fmt::Debug for FolderDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderDb")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// A database error as the engine's: [`Error::CorruptState`] if the file is damaged, and the
/// folder is to be rebuilt; otherwise an I/O error, such as another process holding the file or
/// a permission it lacks, which a rebuild would not mend (ADR 0034).
fn err(path: &Path) -> impl Fn(redb::Error) -> Error + '_ {
    move |e| match e {
        redb::Error::Corrupted(_)
        | redb::Error::TableDoesNotExist(_)
        | redb::Error::TableTypeMismatch { .. }
        | redb::Error::UpgradeRequired(_) => Error::CorruptState(path.to_path_buf()),
        // What redb reports for a file that is not a database, such as one cut short.
        redb::Error::Io(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::InvalidData | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            Error::CorruptState(path.to_path_buf())
        }
        e => Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(e.to_string()),
        },
    }
}

/// Each folder's database caches this much of its file (ADR 0035): redb's default, 1 GiB per
/// database, would let a large folder's load fill memory.
const CACHE_BYTES: usize = 4 << 20;

fn builder() -> redb::Builder {
    let mut builder = Database::builder();
    builder.set_cache_size(CACHE_BYTES);
    builder
}

/// The header key saying the chunk index was filled from the manifests.
const CHUNKS_BUILT: &str = "chunks built";
/// When this database began recording when versions were received (ADR 0040): a version
/// stored before then has no time, and one after without a time was made here or received
/// while rebuilding.
const RECEIPTS_SINCE: &str = "receipts since";

/// The chunk index's rows for `manifest`: each chunk's hash, then the content's.
pub(crate) fn chunk_rows(manifest: &Manifest) -> impl Iterator<Item = [u8; 64]> + '_ {
    manifest.chunks.iter().map(|chunk| {
        let mut row = [0u8; 64];
        row[..32].copy_from_slice(&chunk.hash.0);
        row[32..].copy_from_slice(&manifest.content.0);
        row
    })
}

/// Reads manifests and the chunk index from a folder's database, concurrently with commits
/// (ADR 0035). It sees what is committed; the caller overlays what is not yet.
#[derive(Clone)]
pub(crate) struct Reader {
    db: Arc<RwLock<Database>>,
    path: PathBuf,
    encrypted: bool,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reader").field("path", &self.path).finish()
    }
}

impl Reader {
    fn begin(&self) -> Result<redb::ReadTransaction> {
        let tx = self.db.read().unwrap().begin_read();
        tx.map_err(|x| err(&self.path)(x.into()))
    }

    /// The manifest of `content`, if committed.
    pub(crate) fn manifest(&self, content: &ContentHash) -> Result<Option<Manifest>> {
        let e = |x: redb::Error| err(&self.path)(x);
        let tx = self.begin()?;
        let table = tx.open_table(MANIFESTS).map_err(|x| e(x.into()))?;
        let key = crate::tracked::encode(content);
        match table.get(key.as_slice()).map_err(|x| e(x.into()))? {
            Some(value) => Ok(Some(
                postcard::from_bytes(value.value()).map_err(corrupt(&self.path))?,
            )),
            None => Ok(None),
        }
    }

    /// The committed contents holding chunk `hash`.
    pub(crate) fn contents_with(&self, hash: &ChunkHash) -> Result<Vec<ContentHash>> {
        let e = |x: redb::Error| err(&self.path)(x);
        let tx = self.begin()?;
        let table = tx.open_table(CHUNKS).map_err(|x| e(x.into()))?;
        let mut low = [0u8; 64];
        low[..32].copy_from_slice(&hash.0);
        let mut high = low;
        high[32..].fill(0xff);
        let mut out = Vec::new();
        for row in table
            .range(low.as_slice()..=high.as_slice())
            .map_err(|x| e(x.into()))?
        {
            let (key, _) = row.map_err(|x| e(x.into()))?;
            let mut content = [0u8; 32];
            content.copy_from_slice(&key.value()[32..]);
            out.push(ContentHash(content));
        }
        Ok(out)
    }

    /// When each of the committed versions with `sequences` was received, this device's time,
    /// in that order: none for a version made here, received before this was recorded, or
    /// received while rebuilding, or not there (ADR 0040). Read only when needed, never kept in
    /// memory (ADR 0035).
    pub(crate) fn received(&self, sequences: &[u64]) -> Result<Vec<Option<i64>>> {
        let e = |x: redb::Error| err(&self.path)(x);
        let tx = self.begin()?;
        let table = tx.open_table(VERSIONS).map_err(|x| e(x.into()))?;
        let mut out = Vec::with_capacity(sequences.len());
        for sequence in sequences {
            let time = table
                .get(*sequence)
                .map_err(|x| e(x.into()))?
                .and_then(|value| received_in(value.value(), self.encrypted));
            out.push(time);
        }
        Ok(out)
    }

    /// When this database began recording when versions were received (ADR 0040).
    pub(crate) fn receipts_since(&self) -> Result<Option<i64>> {
        let e = |x: redb::Error| err(&self.path)(x);
        let tx = self.begin()?;
        let table = tx.open_table(HEADER).map_err(|x| e(x.into()))?;
        let since = table.get(RECEIPTS_SINCE).map_err(|x| e(x.into()))?;
        Ok(since.and_then(|v| postcard::from_bytes(v.value()).ok()))
    }

    /// The committed sealed versions with `sequences`, as signed, in that order; one not there
    /// is left out. An encrypted folder's only (ADR 0023).
    pub(crate) fn sealed_versions(&self, sequences: &[u64]) -> Result<Vec<Signed<SealedVersion>>> {
        let e = |x: redb::Error| err(&self.path)(x);
        let tx = self.begin()?;
        let table = tx.open_table(VERSIONS).map_err(|x| e(x.into()))?;
        let mut out = Vec::with_capacity(sequences.len());
        for sequence in sequences {
            if let Some(value) = table.get(*sequence).map_err(|x| e(x.into()))? {
                out.push(postcard::from_bytes(value.value()).map_err(corrupt(&self.path))?);
            }
        }
        Ok(out)
    }

    /// The committed versions with `sequences`, signed, in that order; one not there is left out.
    pub(crate) fn versions(&self, sequences: &[u64]) -> Result<Vec<Signed<EntryVersion>>> {
        let e = |x: redb::Error| err(&self.path)(x);
        let tx = self.begin()?;
        let table = tx.open_table(VERSIONS).map_err(|x| e(x.into()))?;
        let mut out = Vec::with_capacity(sequences.len());
        for sequence in sequences {
            if let Some(value) = table.get(*sequence).map_err(|x| e(x.into()))? {
                out.push(postcard::from_bytes(value.value()).map_err(corrupt(&self.path))?);
            }
        }
        Ok(out)
    }

    /// Every committed manifest's content, for finding those nothing references.
    pub(crate) fn contents(&self) -> Result<Vec<ContentHash>> {
        let e = |x: redb::Error| err(&self.path)(x);
        let tx = self.begin()?;
        let table = tx.open_table(MANIFESTS).map_err(|x| e(x.into()))?;
        let mut out = Vec::new();
        for row in table.iter().map_err(|x| e(x.into()))? {
            let (key, _) = row.map_err(|x| e(x.into()))?;
            out.push(postcard::from_bytes(key.value()).map_err(corrupt(&self.path))?);
        }
        Ok(out)
    }
}

fn corrupt(path: &Path) -> impl Fn(postcard::Error) -> Error + '_ {
    move |_| Error::CorruptState(path.to_path_buf())
}

/// When a versions row says its version was received: the time after the signed version, and
/// after what was decrypted from it in an encrypted folder's.
fn received_in(row: &[u8], encrypted: bool) -> Option<i64> {
    let (_, mut rest) = postcard::take_from_bytes::<Signed<EntryVersion>>(row).ok()?;
    if encrypted {
        rest = postcard::take_from_bytes::<EntryVersion>(rest).ok()?.1;
    }
    postcard::from_bytes(rest).ok()
}

/// A versions row's signed version, and what was decrypted from it in an encrypted folder's.
fn row_version(row: &[u8], encrypted: bool) -> Option<(Wire, Option<EntryVersion>)> {
    if encrypted {
        let (signed, rest) = postcard::take_from_bytes::<Signed<SealedVersion>>(row).ok()?;
        let (version, _) = postcard::take_from_bytes::<EntryVersion>(rest).ok()?;
        Some((Wire::Sealed(signed), Some(version)))
    } else {
        let (signed, _) = postcard::take_from_bytes::<Signed<EntryVersion>>(row).ok()?;
        Some((Wire::Plain(signed), None))
    }
}

#[expect(
    clippy::expect_used,
    reason = "postcard fails to encode only sequences of unknown length, which none of these are"
)]
fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_stdvec(value).expect("in-memory encoding cannot fail")
}

impl FolderDb {
    /// Whether there is a database at `path`.
    pub(crate) fn exists(path: &Path) -> bool {
        path.exists()
    }

    /// Creates a database at `path`, holding nothing yet.
    pub(crate) fn create(path: &Path, encrypted: bool) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(crate::io_err(dir))?;
        }
        let path = path.to_path_buf();
        let db = builder().create(&path).map_err(|e| err(&path)(e.into()))?;
        // Every table exists from the start, so reading never meets a missing one.
        let tx = db.begin_write().map_err(|e| err(&path)(e.into()))?;
        {
            let e = |e: redb::TableError| err(&path)(e.into());
            tx.open_table(HEADER).map_err(e)?;
            tx.open_table(VERSIONS).map_err(e)?;
            tx.open_table(DROPPED).map_err(e)?;
            tx.open_table(RECYCLE).map_err(e)?;
            tx.open_table(CHUNKS).map_err(e)?;
            tx.open_table(HEADER)
                .map_err(e)?
                .insert(CHUNKS_BUILT, [].as_slice())
                .map_err(|x| err(&path)(x.into()))?;
            if encrypted {
                tx.open_table(HEADER)
                    .map_err(e)?
                    .insert(ENCRYPTED, [].as_slice())
                    .map_err(|x| err(&path)(x.into()))?;
            }
            for name in MAPS {
                tx.open_table(TableDefinition::<&[u8], &[u8]>::new(name))
                    .map_err(e)?;
            }
        }
        tx.commit().map_err(|e| err(&path)(e.into()))?;
        Ok(Self {
            db: Arc::new(RwLock::new(db)),
            path,
            live: 0,
            dropped: 0,
            next_drop: 0,
            checked: true,
            encrypted,
        })
    }

    /// Opens the database at `path` and reads everything in it.
    /// Opens the database and reads all of it, checking it first if it is small or `check` is
    /// set: see [`FolderDb::checked`].
    pub(crate) fn open(path: &Path, check: bool) -> Result<(Self, Loaded)> {
        // redb panics on some damage, such as a file cut short.
        crate::diagnostics::catching(|| Self::open_checked(path, check, CHECKED_AT_OPEN))
            .unwrap_or_else(|_| Err(Error::CorruptState(path.to_path_buf())))
    }

    fn open_checked(path: &Path, check: bool, limit: u64) -> Result<(Self, Loaded)> {
        let path = path.to_path_buf();
        let mut db = builder().open(&path).map_err(|e| err(&path)(e.into()))?;
        let checked = check || std::fs::metadata(&path).map_or(true, |m| m.len() <= limit);
        // The database's own check, beyond what opening it does (ADR 0034).
        if checked && !db.check_integrity().map_err(|e| err(&path)(e.into()))? {
            tracing::warn!(path = %path.display(), "the folder's database was damaged and repaired");
        }
        let mut folder = Self {
            db: Arc::new(RwLock::new(db)),
            path,
            live: 0,
            dropped: 0,
            next_drop: 0,
            checked,
            encrypted: false,
        };
        folder.encrypted = folder.is_encrypted()?;
        folder.build_chunk_index()?;
        let (loaded, dropped, next_drop) = folder.read()?;
        folder.live = loaded.versions.len() as u64;
        folder.dropped = dropped;
        folder.next_drop = next_drop;
        Ok((folder, loaded))
    }

    /// Whether the database was checked when opened: if not, it was too large to check quickly,
    /// and is checked soon after.
    pub(crate) fn checked(&self) -> bool {
        self.checked
    }

    /// Whether this is an encrypted folder's database (ADR 0023).
    pub(crate) fn encrypted(&self) -> bool {
        self.encrypted
    }

    fn is_encrypted(&self) -> Result<bool> {
        let e = |e: redb::Error| err(&self.path)(e);
        let tx = self
            .db
            .read()
            .unwrap()
            .begin_read()
            .map_err(|x| e(x.into()))?;
        let table = tx.open_table(HEADER).map_err(|x| e(x.into()))?;
        Ok(table.get(ENCRYPTED).map_err(|x| e(x.into()))?.is_some())
    }

    /// Everything in the database, as [`FolderDb::open`] reads it, for checking saves in
    /// development builds.
    #[cfg(debug_assertions)]
    pub(crate) fn read_all(&self) -> Result<Loaded> {
        self.read().map(|(loaded, _, _)| loaded)
    }

    /// Checks the database file for damage (ADR 0034). Fails with [`Error::CorruptState`] if it
    /// is damaged beyond repair; redb repairs what it can.
    pub(crate) fn check(&mut self) -> Result<()> {
        let path = self.path.clone();
        // The check reads the file as on disk: changes committed without waiting for it, as
        // last-use times are, are written first, or redb sees its own state as damaged.
        let mut db = self.db.write().unwrap();
        let mut tx = db.begin_write().map_err(|e| err(&path)(e.into()))?;
        tx.set_durability(Durability::Immediate)
            .map_err(|e| err(&path)(e.into()))?;
        tx.commit().map_err(|e| err(&path)(e.into()))?;
        let checked = crate::diagnostics::catching(|| db.check_integrity())
            .map_err(|_| Error::CorruptState(path.clone()))?;
        if !checked.map_err(|e| err(&path)(e.into()))? {
            tracing::warn!(path = %path.display(), "the folder's database was damaged and repaired");
        }
        Ok(())
    }

    /// Everything in the database, with how many versions are dropped and the next key of the
    /// dropped table.
    fn read(&self) -> Result<(Loaded, u64, u64)> {
        let path = &self.path;
        let e = |e: redb::Error| err(path)(e);
        let tx = self
            .db
            .read()
            .unwrap()
            .begin_read()
            .map_err(|x| e(x.into()))?;
        let header: HeaderRecord = {
            let table = tx.open_table(HEADER).map_err(|x| e(x.into()))?;
            let bytes = table
                .get("header")
                .map_err(|x| e(x.into()))?
                .ok_or_else(|| Error::CorruptState(path.clone()))?;
            postcard::from_bytes(bytes.value()).map_err(corrupt(path))?
        };
        if header.format != FORMAT {
            return Err(Error::CorruptState(path.clone()));
        }
        let rebuild: Option<Rebuild> = {
            let table = tx.open_table(HEADER).map_err(|x| e(x.into()))?;
            match table.get("rebuild").map_err(|x| e(x.into()))? {
                Some(bytes) => postcard::from_bytes(bytes.value()).map_err(corrupt(path))?,
                None => None,
            }
        };
        let mut gone = std::collections::BTreeSet::new();
        let mut last_drop = None;
        for row in tx
            .open_table(DROPPED)
            .map_err(|x| e(x.into()))?
            .iter()
            .map_err(|x| e(x.into()))?
        {
            let (key, value) = row.map_err(|x| e(x.into()))?;
            last_drop = Some(key.value());
            let sequences: Vec<u64> = postcard::from_bytes(value.value()).map_err(corrupt(path))?;
            gone.extend(sequences);
        }
        let mut versions = Vec::new();
        for row in tx
            .open_table(VERSIONS)
            .map_err(|x| e(x.into()))?
            .iter()
            .map_err(|x| e(x.into()))?
        {
            let (key, value) = row.map_err(|x| e(x.into()))?;
            let sequence = key.value();
            if gone.contains(&sequence) {
                continue;
            }
            let (signed, decrypted) = row_version(value.value(), self.encrypted)
                .ok_or_else(|| Error::CorruptState(path.clone()))?;
            versions.push((sequence, signed, decrypted));
        }
        let mut recycled = Vec::new();
        for row in tx
            .open_table(RECYCLE)
            .map_err(|x| e(x.into()))?
            .iter()
            .map_err(|x| e(x.into()))?
        {
            let (_, value) = row.map_err(|x| e(x.into()))?;
            recycled.push(postcard::from_bytes(value.value()).map_err(corrupt(path))?);
        }
        let mut maps = BTreeMap::new();
        // Manifests stay on disk, read one at a time (ADR 0035).
        for name in MAPS.into_iter().filter(|name| *name != "manifests") {
            let mut records = Vec::new();
            let table = match tx.open_table(TableDefinition::<&[u8], &[u8]>::new(name)) {
                Ok(table) => table,
                // A table added since the database was made: none of its records yet.
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    maps.insert(name, records);
                    continue;
                }
                Err(x) => return Err(e(x.into())),
            };
            for row in table.iter().map_err(|x| e(x.into()))? {
                let (key, value) = row.map_err(|x| e(x.into()))?;
                records.push((key.value().to_vec(), value.value().to_vec()));
            }
            maps.insert(name, records);
        }
        let loaded = Loaded {
            rebuild,
            header: Header {
                id: header.folder,
                device: header.device,
                epoch: header.epoch,
                sequence: header.sequence,
                retention: header.retention,
            },
            versions,
            recycled,
            maps,
        };
        Ok((
            loaded,
            gone.len() as u64,
            last_drop.map_or(0, |k| k.saturating_add(1)),
        ))
    }

    /// Writes `changes` in one transaction, then removes dropped versions if it is time.
    pub(crate) fn commit(&mut self, changes: &Changes) -> Result<()> {
        let path = self.path.clone();
        let e = |x: redb::Error| err(&path)(x);
        let mut tx = self
            .db
            .read()
            .unwrap()
            .begin_write()
            .map_err(|x| e(x.into()))?;
        tx.set_durability(if changes.durable {
            Durability::Immediate
        } else {
            Durability::None
        })
        .map_err(|x| e(x.into()))?;
        let Changes {
            header,
            journal,
            maps,
            ..
        } = changes;
        let (stored, dropped) = (journal.stored.len() as u64, journal.dropped.len() as u64);
        {
            let mut table = tx.open_table(HEADER).map_err(|x| e(x.into()))?;
            let record = HeaderRecord {
                format: FORMAT,
                folder: header.id,
                device: header.device,
                epoch: header.epoch,
                sequence: header.sequence,
                retention: header.retention,
            };
            table
                .insert("header", encode(&record).as_slice())
                .map_err(|x| e(x.into()))?;
            table
                .insert("rebuild", encode(&changes.rebuild).as_slice())
                .map_err(|x| e(x.into()))?;
            if table
                .get(RECEIPTS_SINCE)
                .map_err(|x| e(x.into()))?
                .is_none()
            {
                table
                    .insert(RECEIPTS_SINCE, encode(&changes.at_ms).as_slice())
                    .map_err(|x| e(x.into()))?;
            }
        }
        if !journal.stored.is_empty() {
            let received: BTreeMap<u64, i64> = journal.received.iter().copied().collect();
            let decrypted: BTreeMap<u64, &EntryVersion> =
                journal.decrypted.iter().map(|(s, v)| (*s, v)).collect();
            let mut table = tx.open_table(VERSIONS).map_err(|x| e(x.into()))?;
            for (sequence, signed) in &journal.stored {
                // When it was received follows the version, where readers from before it, such
                // as 0.19, stop reading. A sealed version is followed by what was decrypted
                // from it, before that (ADR 0023).
                let mut row = match signed {
                    Wire::Plain(signed) => encode(signed),
                    Wire::Sealed(signed) => {
                        let mut row = encode(signed);
                        let version = decrypted
                            .get(sequence)
                            .ok_or_else(|| Error::CorruptState(self.path.clone()))?;
                        row.extend(encode(*version));
                        row
                    }
                };
                if let Some(at) = received.get(sequence) {
                    row.extend(encode(at));
                }
                table
                    .insert(*sequence, row.as_slice())
                    .map_err(|x| e(x.into()))?;
            }
        }
        if !journal.dropped.is_empty() {
            let mut table = tx.open_table(DROPPED).map_err(|x| e(x.into()))?;
            table
                .insert(self.next_drop, encode(&journal.dropped).as_slice())
                .map_err(|x| e(x.into()))?;
        }
        if !journal.recycle.is_empty() {
            let mut table = tx.open_table(RECYCLE).map_err(|x| e(x.into()))?;
            for (entry, item) in &journal.recycle {
                match item {
                    Some(item) => {
                        table
                            .insert(entry.0.as_slice(), encode(item).as_slice())
                            .map_err(|x| e(x.into()))?;
                    }
                    None => {
                        table.remove(entry.0.as_slice()).map_err(|x| e(x.into()))?;
                    }
                }
            }
        }
        for (name, records) in maps {
            if records.is_empty() {
                continue;
            }
            let mut table = tx
                .open_table(TableDefinition::<&[u8], &[u8]>::new(name))
                .map_err(|x| e(x.into()))?;
            for (key, value) in records {
                match value {
                    Some(value) => {
                        table
                            .insert(key.as_slice(), value.as_slice())
                            .map_err(|x| e(x.into()))?;
                    }
                    None => {
                        table.remove(key.as_slice()).map_err(|x| e(x.into()))?;
                    }
                }
            }
        }
        if !changes.chunks.is_empty() {
            let mut table = tx.open_table(CHUNKS).map_err(|x| e(x.into()))?;
            for (row, add) in &changes.chunks {
                if *add {
                    table.insert(row.as_slice(), ()).map_err(|x| e(x.into()))?;
                } else {
                    table.remove(row.as_slice()).map_err(|x| e(x.into()))?;
                }
            }
        }
        tx.commit().map_err(|x| e(x.into()))?;
        if dropped > 0 {
            self.next_drop = self.next_drop.saturating_add(1);
        }
        self.live = self.live.saturating_add(stored).saturating_sub(dropped);
        self.dropped = self.dropped.saturating_add(dropped);
        if self.dropped >= CLEAN_AT_LEAST && self.dropped >= self.live {
            self.clean()?;
        }
        Ok(())
    }

    /// Fills the chunk index from the manifests, once, for a database made before it existed.
    fn build_chunk_index(&mut self) -> Result<()> {
        let path = self.path.clone();
        let e = |x: redb::Error| err(&path)(x);
        let db = self.db.read().unwrap();
        let tx = db.begin_write().map_err(|x| e(x.into()))?;
        {
            let mut header = tx.open_table(HEADER).map_err(|x| e(x.into()))?;
            if header.get(CHUNKS_BUILT).map_err(|x| e(x.into()))?.is_some() {
                return Ok(());
            }
            header
                .insert(CHUNKS_BUILT, [].as_slice())
                .map_err(|x| e(x.into()))?;
            let manifests = tx.open_table(MANIFESTS).map_err(|x| e(x.into()))?;
            let mut chunks = tx.open_table(CHUNKS).map_err(|x| e(x.into()))?;
            for row in manifests.iter().map_err(|x| e(x.into()))? {
                let (_, value) = row.map_err(|x| e(x.into()))?;
                let manifest: Manifest =
                    postcard::from_bytes(value.value()).map_err(corrupt(&path))?;
                for row in chunk_rows(&manifest) {
                    chunks.insert(row.as_slice(), ()).map_err(|x| e(x.into()))?;
                }
            }
        }
        tx.commit().map_err(|x| e(x.into()))?;
        Ok(())
    }

    /// A handle for reading manifests and the chunk index while the committer writes.
    pub(crate) fn reader(&self) -> Reader {
        Reader {
            db: self.db.clone(),
            path: self.path.clone(),
            encrypted: self.encrypted,
        }
    }

    /// Removes the dropped versions in one sorted pass, then gives the freed space back.
    fn clean(&mut self) -> Result<()> {
        let path = self.path.clone();
        let e = |x: redb::Error| err(&path)(x);
        let tx = self
            .db
            .read()
            .unwrap()
            .begin_write()
            .map_err(|x| e(x.into()))?;
        {
            let mut gone = Vec::new();
            {
                let dropped = tx.open_table(DROPPED).map_err(|x| e(x.into()))?;
                for row in dropped.iter().map_err(|x| e(x.into()))? {
                    let (_, value) = row.map_err(|x| e(x.into()))?;
                    let sequences: Vec<u64> =
                        postcard::from_bytes(value.value()).map_err(corrupt(&path))?;
                    gone.extend(sequences);
                }
            }
            gone.sort_unstable();
            let mut versions = tx.open_table(VERSIONS).map_err(|x| e(x.into()))?;
            for sequence in gone {
                versions.remove(sequence).map_err(|x| e(x.into()))?;
            }
            let mut dropped = tx.open_table(DROPPED).map_err(|x| e(x.into()))?;
            dropped.retain(|_, _| false).map_err(|x| e(x.into()))?;
        }
        tx.commit().map_err(|x| e(x.into()))?;
        self.dropped = 0;
        self.next_drop = 0;
        while self
            .db
            .write()
            .unwrap()
            .compact()
            .map_err(|x| e(x.into()))?
        {}
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidefell_proto::EntryId;

    fn header(sequence: u64) -> Header {
        Header {
            id: FolderId([1; 32]),
            device: DeviceId([2; 32]),
            epoch: 7,
            sequence,
            retention: Retention::Days(30),
        }
    }

    #[test]
    fn a_large_database_is_checked_after_it_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.redb");
        let mut db = FolderDb::create(&path, false).unwrap();
        db.commit(&Changes {
            header: header(0),
            journal: Journal::default(),
            maps: Vec::new(),
            durable: true,
            rebuild: None,
            activity: Vec::new(),
            at_ms: 0,
            chunks: Vec::new(),
            generation: 0,
        })
        .unwrap();
        drop(db);
        // Over the size checked when opened: opened unchecked, unless a check is asked for.
        let (db, _) = FolderDb::open_checked(&path, false, 0).unwrap();
        assert!(!db.checked());
        drop(db);
        let (db, _) = FolderDb::open_checked(&path, true, 0).unwrap();
        assert!(db.checked());
        drop(db);
        assert!(FolderDb::open(&path, false).unwrap().0.checked());
    }

    #[test]
    fn a_sound_database_passes_its_check() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = FolderDb::create(&dir.path().join("f.redb"), false).unwrap();
        db.commit(&Changes {
            header: header(0),
            journal: Journal::default(),
            maps: Vec::new(),
            durable: true,
            rebuild: None,
            activity: Vec::new(),
            at_ms: 0,
            chunks: Vec::new(),
            generation: 0,
        })
        .unwrap();
        db.check().unwrap();
        // Changes not yet on disk, as last-use times are committed.
        for sequence in 1..4 {
            db.commit(&Changes {
                header: header(sequence),
                journal: Journal::default(),
                maps: Vec::new(),
                durable: false,
                rebuild: None,
                activity: Vec::new(),
                at_ms: 0,
                chunks: Vec::new(),
                generation: 0,
            })
            .unwrap();
        }
        db.check().unwrap();
        db.commit(&Changes {
            header: header(9),
            journal: Journal::default(),
            maps: Vec::new(),
            durable: false,
            rebuild: None,
            activity: Vec::new(),
            at_ms: 0,
            chunks: Vec::new(),
            generation: 0,
        })
        .unwrap();
        assert_eq!(db.read().unwrap().0.header.sequence, 9);
    }

    #[test]
    fn a_damaged_file_is_corrupt_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.redb");
        let mut db = FolderDb::create(&path, false).unwrap();
        db.commit(&Changes {
            header: header(0),
            journal: Journal::default(),
            maps: Vec::new(),
            durable: true,
            rebuild: None,
            activity: Vec::new(),
            at_ms: 0,
            chunks: Vec::new(),
            generation: 0,
        })
        .unwrap();
        drop(db);
        let whole = std::fs::read(&path).unwrap();
        let damaged = [
            vec![0xab; whole.len()],
            whole[..whole.len() / 2].to_vec(),
            Vec::new(),
        ];
        for bytes in damaged {
            std::fs::write(&path, &bytes).unwrap();
            match FolderDb::open(&path, true) {
                Err(Error::CorruptState(_)) => {}
                other => panic!("{} bytes: {:?}", bytes.len(), other.map(|_| ())),
            }
        }
    }

    #[test]
    fn what_was_committed_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = FolderDb::create(&dir.path().join("f.redb"), false).unwrap();
        let key = tidefell_identity::DeviceKey::from_seed([3; 32]);
        let key = key.signing_key();
        let version = |n: u8| EntryVersion {
            folder: FolderId([1; 32]),
            entry: EntryId([n; 16]),
            path: format!("f{n}"),
            kind: tidefell_proto::sync::EntryKind::Directory,
            modified_ms: 0,
            author: DeviceId(key.verifying_key().to_bytes()),
            version: tidefell_proto::VersionVector::new(),
        };
        let signed = |n: u8| Signed::sign(&version(n), key);
        db.commit(&Changes {
            header: header(2),
            journal: Journal {
                stored: vec![(1, Wire::Plain(signed(1))), (2, Wire::Plain(signed(2)))],
                ..Journal::default()
            },
            maps: vec![("pins", vec![(b"a".to_vec(), Some(Vec::new()))])],
            durable: true,
            rebuild: None,
            activity: Vec::new(),
            at_ms: 0,
            chunks: Vec::new(),
            generation: 0,
        })
        .unwrap();
        db.commit(&Changes {
            header: header(3),
            journal: Journal {
                stored: vec![(3, Wire::Plain(signed(3)))],
                dropped: vec![1],
                ..Journal::default()
            },
            maps: vec![(
                "pins",
                vec![(b"a".to_vec(), None), (b"b".to_vec(), Some(Vec::new()))],
            )],
            durable: false,
            rebuild: None,
            activity: Vec::new(),
            at_ms: 0,
            chunks: Vec::new(),
            generation: 0,
        })
        .unwrap();
        drop(db);
        let (db, loaded) = FolderDb::open(&dir.path().join("f.redb"), true).unwrap();
        assert_eq!(loaded.header, header(3));
        let sequences: Vec<u64> = loaded.versions.iter().map(|(s, _, _)| *s).collect();
        assert_eq!(sequences, [2, 3]);
        assert_eq!(loaded.maps["pins"], [(b"b".to_vec(), Vec::new())]);
        assert_eq!((db.live, db.dropped, db.next_drop), (2, 1, 1));
    }

    #[test]
    fn dropped_versions_are_removed_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = FolderDb::create(&dir.path().join("f.redb"), false).unwrap();
        let key = tidefell_identity::DeviceKey::from_seed([3; 32]);
        let key = key.signing_key();
        let signed = |n: u64| {
            Signed::sign(
                &EntryVersion {
                    folder: FolderId([1; 32]),
                    entry: EntryId([0; 16]),
                    path: format!("f{n}"),
                    kind: tidefell_proto::sync::EntryKind::Directory,
                    modified_ms: 0,
                    author: DeviceId(key.verifying_key().to_bytes()),
                    version: tidefell_proto::VersionVector::new(),
                },
                key,
            )
        };
        let n = CLEAN_AT_LEAST * 2;
        db.commit(&Changes {
            header: header(n),
            journal: Journal {
                stored: (1..=n).map(|s| (s, Wire::Plain(signed(s)))).collect(),
                ..Journal::default()
            },
            maps: Vec::new(),
            durable: true,
            rebuild: None,
            activity: Vec::new(),
            at_ms: 0,
            chunks: Vec::new(),
            generation: 0,
        })
        .unwrap();
        db.commit(&Changes {
            header: header(n),
            journal: Journal {
                dropped: (1..=CLEAN_AT_LEAST).collect(),
                ..Journal::default()
            },
            maps: Vec::new(),
            durable: true,
            rebuild: None,
            activity: Vec::new(),
            at_ms: 0,
            chunks: Vec::new(),
            generation: 0,
        })
        .unwrap();
        // As many dropped as live: removed, and the file compacted.
        assert_eq!((db.live, db.dropped), (CLEAN_AT_LEAST, 0));
        drop(db);
        let (_, loaded) = FolderDb::open(&dir.path().join("f.redb"), true).unwrap();
        assert_eq!(loaded.versions.len() as u64, CLEAN_AT_LEAST);
        assert_eq!(loaded.versions[0].0, CLEAN_AT_LEAST + 1);
    }
}
