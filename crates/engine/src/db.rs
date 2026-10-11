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

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};
use tidefell_meta::{Header, Journal, Recycled, Retention};
use tidefell_proto::sync::EntryVersion;
use tidefell_proto::{DeviceId, FolderId, Signed};

use crate::tracked::Changed;
use crate::{Error, Result};

/// The layout of the tables below; a database of another format is not read.
const FORMAT: u32 = 1;

const HEADER: TableDefinition<&str, &[u8]> = TableDefinition::new("header");
const VERSIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("versions");
const DROPPED: TableDefinition<u64, &[u8]> = TableDefinition::new("dropped");
const RECYCLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("recycle");

/// The engine's records, one table each, by name.
pub(crate) const MAPS: [&str; 11] = [
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
];

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

/// What to write in one transaction.
pub(crate) struct Changes {
    pub(crate) header: Header,
    pub(crate) journal: Journal,
    /// By table name, from [`MAPS`].
    pub(crate) maps: Vec<(&'static str, Vec<Changed>)>,
    /// Wait for the disk before returning: for changes peers may be told about, or that the
    /// engine cannot make again, such as signed versions and index progress.
    pub(crate) durable: bool,
    /// What the changes did, for the activity log (ADR 0030), written once they are committed.
    pub(crate) activity: Vec<tidefell_meta::Change>,
    /// When they were applied.
    pub(crate) at_ms: i64,
}

/// What a database holds.
pub(crate) struct Loaded {
    pub(crate) header: Header,
    /// The versions still kept, in sequence order.
    pub(crate) versions: Vec<(u64, Signed<EntryVersion>)>,
    pub(crate) recycled: Vec<Recycled>,
    /// By table name, encoded keys and values.
    pub(crate) maps: BTreeMap<&'static str, Records>,
}

/// One folder's database.
pub(crate) struct FolderDb {
    db: Database,
    path: PathBuf,
    /// Versions kept.
    live: u64,
    /// Versions dropped but not yet removed.
    dropped: u64,
    /// The next key of the dropped table.
    next_drop: u64,
}

impl std::fmt::Debug for FolderDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FolderDb")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn err(path: &Path) -> impl Fn(redb::Error) -> Error + '_ {
    move |e| Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::other(e.to_string()),
    }
}

fn corrupt(path: &Path) -> impl Fn(postcard::Error) -> Error + '_ {
    move |_| Error::CorruptState(path.to_path_buf())
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
    pub(crate) fn create(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(crate::io_err(dir))?;
        }
        let path = path.to_path_buf();
        let db = Database::create(&path).map_err(|e| err(&path)(e.into()))?;
        // Every table exists from the start, so reading never meets a missing one.
        let tx = db.begin_write().map_err(|e| err(&path)(e.into()))?;
        {
            let e = |e: redb::TableError| err(&path)(e.into());
            tx.open_table(HEADER).map_err(e)?;
            tx.open_table(VERSIONS).map_err(e)?;
            tx.open_table(DROPPED).map_err(e)?;
            tx.open_table(RECYCLE).map_err(e)?;
            for name in MAPS {
                tx.open_table(TableDefinition::<&[u8], &[u8]>::new(name))
                    .map_err(e)?;
            }
        }
        tx.commit().map_err(|e| err(&path)(e.into()))?;
        Ok(Self {
            db,
            path,
            live: 0,
            dropped: 0,
            next_drop: 0,
        })
    }

    /// Opens the database at `path` and reads everything in it.
    pub(crate) fn open(path: &Path) -> Result<(Self, Loaded)> {
        let path = path.to_path_buf();
        let db = Database::open(&path).map_err(|e| err(&path)(e.into()))?;
        let mut folder = Self {
            db,
            path,
            live: 0,
            dropped: 0,
            next_drop: 0,
        };
        let (loaded, dropped, next_drop) = folder.read()?;
        folder.live = loaded.versions.len() as u64;
        folder.dropped = dropped;
        folder.next_drop = next_drop;
        Ok((folder, loaded))
    }

    /// Everything in the database, as [`FolderDb::open`] reads it, for checking saves in
    /// development builds.
    #[cfg(debug_assertions)]
    pub(crate) fn read_all(&self) -> Result<Loaded> {
        self.read().map(|(loaded, _, _)| loaded)
    }

    /// Everything in the database, with how many versions are dropped and the next key of the
    /// dropped table.
    fn read(&self) -> Result<(Loaded, u64, u64)> {
        let path = &self.path;
        let e = |e: redb::Error| err(path)(e);
        let tx = self.db.begin_read().map_err(|x| e(x.into()))?;
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
            let signed: Signed<EntryVersion> =
                postcard::from_bytes(value.value()).map_err(corrupt(path))?;
            versions.push((sequence, signed));
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
        for name in MAPS {
            let mut records = Vec::new();
            for row in tx
                .open_table(TableDefinition::<&[u8], &[u8]>::new(name))
                .map_err(|x| e(x.into()))?
                .iter()
                .map_err(|x| e(x.into()))?
            {
                let (key, value) = row.map_err(|x| e(x.into()))?;
                records.push((key.value().to_vec(), value.value().to_vec()));
            }
            maps.insert(name, records);
        }
        let loaded = Loaded {
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
        let mut tx = self.db.begin_write().map_err(|x| e(x.into()))?;
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
        }
        if !journal.stored.is_empty() {
            let mut table = tx.open_table(VERSIONS).map_err(|x| e(x.into()))?;
            for (sequence, signed) in &journal.stored {
                table
                    .insert(*sequence, encode(signed).as_slice())
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

    /// Removes the dropped versions in one sorted pass, then gives the freed space back.
    fn clean(&mut self) -> Result<()> {
        let path = self.path.clone();
        let e = |x: redb::Error| err(&path)(x);
        let tx = self.db.begin_write().map_err(|x| e(x.into()))?;
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
        while self.db.compact().map_err(|x| e(x.into()))? {}
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
    fn what_was_committed_is_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = FolderDb::create(&dir.path().join("f.redb")).unwrap();
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
                stored: vec![(1, signed(1)), (2, signed(2))],
                ..Journal::default()
            },
            maps: vec![("pins", vec![(b"a".to_vec(), Some(Vec::new()))])],
            durable: true,
            activity: Vec::new(),
            at_ms: 0,
        })
        .unwrap();
        db.commit(&Changes {
            header: header(3),
            journal: Journal {
                stored: vec![(3, signed(3))],
                dropped: vec![1],
                ..Journal::default()
            },
            maps: vec![(
                "pins",
                vec![(b"a".to_vec(), None), (b"b".to_vec(), Some(Vec::new()))],
            )],
            durable: false,
            activity: Vec::new(),
            at_ms: 0,
        })
        .unwrap();
        drop(db);
        let (db, loaded) = FolderDb::open(&dir.path().join("f.redb")).unwrap();
        assert_eq!(loaded.header, header(3));
        let sequences: Vec<u64> = loaded.versions.iter().map(|(s, _)| *s).collect();
        assert_eq!(sequences, [2, 3]);
        assert_eq!(loaded.maps["pins"], [(b"b".to_vec(), Vec::new())]);
        assert_eq!((db.live, db.dropped, db.next_drop), (2, 1, 1));
    }

    #[test]
    fn dropped_versions_are_removed_in_one_pass() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = FolderDb::create(&dir.path().join("f.redb")).unwrap();
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
                stored: (1..=n).map(|s| (s, signed(s))).collect(),
                ..Journal::default()
            },
            maps: Vec::new(),
            durable: true,
            activity: Vec::new(),
            at_ms: 0,
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
            activity: Vec::new(),
            at_ms: 0,
        })
        .unwrap();
        // As many dropped as live: removed, and the file compacted.
        assert_eq!((db.live, db.dropped), (CLEAN_AT_LEAST, 0));
        drop(db);
        let (_, loaded) = FolderDb::open(&dir.path().join("f.redb")).unwrap();
        assert_eq!(loaded.versions.len() as u64, CLEAN_AT_LEAST);
        assert_eq!(loaded.versions[0].0, CLEAN_AT_LEAST + 1);
    }
}
