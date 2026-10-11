//! What saving one folder's metadata costs, today and in a database (ADR 0034 step 2).
//!
//! For each size, a folder of that many entries is built from signed versions, then a batch of
//! 512 changed versions is applied and saved, as an index batch from a peer is:
//!
//! - today: the whole `meta` snapshot rewritten (the engine's `state` file comes on top);
//! - redb and SQLite: the 512 records written in one durable transaction.
//!
//! Bytes written are the process's own, from `/proc/self/io`, so they include what each database
//! writes for its journal. Run with `cargo run --release -- 10000 100000 1000000`.

use std::path::Path;
use std::time::{Duration, Instant};

use redb::{Database, Durability, ReadableDatabase, TableDefinition};
use tidefell_identity::DeviceKey;
use tidefell_meta::{Folder, Retention};
use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::{ContentHash, EntryId, FolderId, Signed};

const BATCH: usize = 512;
const VERSIONS: TableDefinition<&[u8; 16], &[u8]> = TableDefinition::new("versions");
const BY_SEQUENCE: TableDefinition<u64, &[u8; 16]> = TableDefinition::new("by_sequence");
/// Versions by the sequence this device stored them under: a batch appends.
const LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("log");

/// Bytes this process has written to storage so far.
fn written() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .ok()
        .and_then(|io| {
            io.lines()
                .find_map(|l| l.strip_prefix("write_bytes: ")?.trim().parse().ok())
        })
        .unwrap_or(0)
}

fn entry(n: usize) -> EntryId {
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&(n as u64).to_le_bytes());
    EntryId(id)
}

fn file(n: usize, round: u8) -> EntryKind {
    let mut content = [round; 32];
    content[..8].copy_from_slice(&(n as u64).to_le_bytes());
    EntryKind::File {
        content: ContentHash(content),
        size: 100_000 + n as u64,
        executable: false,
    }
}

/// A folder of `size` files, as one device made them.
fn folder(key: &DeviceKey, size: usize) -> Folder {
    let id = FolderId([7; 32]);
    let mut folder = Folder::new(id, key.id(), 1, Retention::Days(30));
    for n in 0..size {
        let path = format!("photos/{:04}/{:02}/IMG_{n:07}.jpg", 2000 + n % 25, n % 12);
        folder
            .local_change(
                key.signing_key(),
                entry(n),
                &path,
                file(n, 0),
                1_700_000_000_000,
            )
            .unwrap();
    }
    folder
}

/// A batch of changes to `BATCH` existing files, signed by another device.
fn batch(other: &DeviceKey, folder: &Folder, size: usize, round: u8) -> Vec<Signed<EntryVersion>> {
    (0..BATCH)
        .map(|i| {
            let n = (i * 7919 + round as usize * 104_729) % size;
            let current = folder.current(&entry(n)).unwrap().clone();
            let mut version = folder.current_vector(&entry(n));
            version.increment(other.id());
            let changed = EntryVersion {
                kind: file(n, round),
                author: other.id(),
                version,
                ..current
            };
            Signed::sign(&changed, other.signing_key())
        })
        .collect()
}

fn measure(what: &str, size: usize, run: impl FnOnce() -> u64) {
    let before = written();
    let start = Instant::now();
    let file_size = run();
    let elapsed = start.elapsed();
    let bytes = written().saturating_sub(before);
    println!(
        "{what:>8} {size:>9} entries: batch of {BATCH} in {:>8.1} ms, {:>10.2} MB written, file {:>8.1} MB",
        ms(elapsed),
        bytes as f64 / 1e6,
        file_size as f64 / 1e6,
    );
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn today(dir: &Path, key: &DeviceKey, other: &DeviceKey, size: usize) {
    let mut folder = folder(key, size);
    let path = dir.join("meta");
    folder.save(&path).unwrap();
    let versions = batch(other, &folder, size, 1);
    measure("today", size, || {
        for signed in versions {
            folder.apply_remote(signed, 1_700_000_000_000).unwrap();
        }
        folder.save(&path).unwrap();
        std::fs::metadata(&path).unwrap().len()
    });
}

fn encoded(folder: &Folder) -> Vec<(EntryId, u64, Vec<u8>)> {
    folder
        .index_since(1, 0, usize::MAX)
        .0
        .into_iter()
        .map(|s| {
            (
                s.version.entry,
                s.sequence,
                postcard::to_allocvec(&s.signed).unwrap(),
            )
        })
        .collect()
}

fn redb(dir: &Path, key: &DeviceKey, other: &DeviceKey, size: usize) {
    let mut folder = folder(key, size);
    let path = dir.join("folder.redb");
    let db = Database::create(&path).unwrap();
    {
        let mut tx = db.begin_write().unwrap();
        tx.set_durability(Durability::Immediate).unwrap();
        {
            let mut versions = tx.open_table(VERSIONS).unwrap();
            let mut by_sequence = tx.open_table(BY_SEQUENCE).unwrap();
            for (entry, sequence, bytes) in encoded(&folder) {
                versions.insert(&entry.0, bytes.as_slice()).unwrap();
                by_sequence.insert(sequence, &entry.0).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    let versions = batch(other, &folder, size, 1);
    measure("redb", size, || {
        let mut changed = Vec::new();
        for signed in versions {
            let entry = signed.verify_authored().unwrap().entry;
            folder
                .apply_remote(signed.clone(), 1_700_000_000_000)
                .unwrap();
            changed.push((
                entry,
                folder.sequence(),
                postcard::to_allocvec(&signed).unwrap(),
            ));
        }
        let mut tx = db.begin_write().unwrap();
        tx.set_durability(Durability::Immediate).unwrap();
        {
            let mut table = tx.open_table(VERSIONS).unwrap();
            let mut by_sequence = tx.open_table(BY_SEQUENCE).unwrap();
            for (entry, sequence, bytes) in &changed {
                table.insert(&entry.0, bytes.as_slice()).unwrap();
                by_sequence.insert(*sequence, &entry.0).unwrap();
            }
        }
        tx.commit().unwrap();
        std::fs::metadata(&path).unwrap().len()
    });
    // Reading one record back, as a lookup would.
    let tx = db.begin_read().unwrap();
    let table = tx.open_table(VERSIONS).unwrap();
    assert!(table.get(&entry(size / 2).0).unwrap().is_some());
}

/// Batches applied in a row for the amortised figures.
const ROUNDS: u32 = 400;

/// A store of versions keyed by the sequence this device stored them under, so a batch appends
/// at the end; which sequence holds each entry's current version is kept in memory and rebuilt
/// on open, and superseded versions are removed in bulk, in sequence order, once there are as
/// many as live ones.
trait Log {
    const NAME: &'static str;
    fn open(dir: &Path) -> Self;
    fn append(&mut self, records: &[(u64, Vec<u8>)], durable: bool);
    fn remove(&mut self, sequences: &[u64]);
    fn size(&self) -> u64;
    fn compact(&mut self);
}

struct RedbLog(Database, std::path::PathBuf);

impl Log for RedbLog {
    const NAME: &'static str = "redb";
    fn open(dir: &Path) -> Self {
        let path = dir.join("folder.redb");
        Self(Database::create(&path).unwrap(), path)
    }
    fn append(&mut self, records: &[(u64, Vec<u8>)], durable: bool) {
        let mut tx = self.0.begin_write().unwrap();
        tx.set_durability(if durable {
            Durability::Immediate
        } else {
            Durability::None
        })
        .unwrap();
        {
            let mut log = tx.open_table(LOG).unwrap();
            for (sequence, bytes) in records {
                log.insert(*sequence, bytes.as_slice()).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    fn remove(&mut self, sequences: &[u64]) {
        let tx = self.0.begin_write().unwrap();
        {
            let mut log = tx.open_table(LOG).unwrap();
            for sequence in sequences {
                log.remove(*sequence).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    fn size(&self) -> u64 {
        std::fs::metadata(&self.1).unwrap().len()
    }
    fn compact(&mut self) {
        while self.0.compact().unwrap() {}
    }
}

struct SqliteLog(rusqlite::Connection, std::path::PathBuf);

impl Log for SqliteLog {
    const NAME: &'static str = "sqlite";
    fn open(dir: &Path) -> Self {
        let path = dir.join("folder.sqlite");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE log (sequence INTEGER PRIMARY KEY, signed BLOB NOT NULL);",
        )
        .unwrap();
        Self(db, path)
    }
    fn append(&mut self, records: &[(u64, Vec<u8>)], durable: bool) {
        self.0
            .pragma_update(None, "synchronous", if durable { "FULL" } else { "OFF" })
            .unwrap();
        let tx = self.0.transaction().unwrap();
        {
            let mut insert = tx.prepare("INSERT INTO log VALUES (?1, ?2)").unwrap();
            for (sequence, bytes) in records {
                insert.execute((*sequence as i64, bytes)).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    fn remove(&mut self, sequences: &[u64]) {
        let tx = self.0.transaction().unwrap();
        {
            let mut delete = tx.prepare("DELETE FROM log WHERE sequence = ?1").unwrap();
            for sequence in sequences {
                delete.execute([*sequence as i64]).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    fn size(&self) -> u64 {
        let wal = self.1.with_extension("sqlite-wal");
        std::fs::metadata(&self.1).unwrap().len() + std::fs::metadata(wal).map_or(0, |m| m.len())
    }
    fn compact(&mut self) {
        self.0
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")
            .unwrap();
    }
}

fn amortised<L: Log>(dir: &Path, key: &DeviceKey, other: &DeviceKey, size: usize) {
    let mut folder = folder(key, size);
    let mut log = L::open(dir);
    let records = encoded(&folder);
    let mut at: std::collections::HashMap<EntryId, u64> =
        records.iter().map(|(e, s, _)| (*e, *s)).collect();
    let initial: Vec<(u64, Vec<u8>)> = records.into_iter().map(|(_, s, b)| (s, b)).collect();
    log.append(&initial, true);
    let loaded = log.size();

    let (mut slowest, mut total_time) = (Duration::ZERO, Duration::ZERO);
    let mut superseded = Vec::new();
    let before = written();
    let mut cleanups = 0;
    for round in 1..=ROUNDS {
        let versions = batch(other, &folder, size, (round % 251) as u8);
        // Checked before the folder is locked; only storing them is timed.
        let verified: Vec<_> = versions
            .into_iter()
            .map(|signed| (signed.verify_authored().unwrap().entry, signed))
            .collect();
        let start = Instant::now();
        let mut appended = Vec::with_capacity(BATCH);
        for (entry, signed) in verified {
            folder
                .apply_remote(signed.clone(), 1_700_000_000_000)
                .unwrap();
            let sequence = folder.sequence();
            superseded.extend(at.insert(entry, sequence));
            appended.push((sequence, postcard::to_allocvec(&signed).unwrap()));
        }
        log.append(&appended, true);
        let took = start.elapsed();
        slowest = slowest.max(took);
        total_time += took;
        if superseded.len() >= size {
            superseded.sort_unstable();
            log.remove(&superseded);
            superseded.clear();
            cleanups += 1;
        }
    }
    let per_batch = written().saturating_sub(before) as f64 / f64::from(ROUNDS);
    println!("{:>6} {size:>9} entries: {cleanups} clean-up(s)", L::NAME);
    let grown = log.size();
    log.compact();
    println!(
        "{:>6} {size:>9} entries: {ROUNDS} batches of {BATCH}, mean {:>6.1} ms, slowest {:>6.1} ms, \
         {:>5.2} MB written per batch with clean-up; file {:.1} MB loaded, {:.1} MB after, {:.1} MB compacted",
        L::NAME,
        ms(total_time) / f64::from(ROUNDS),
        ms(slowest),
        per_batch / 1e6,
        loaded as f64 / 1e6,
        grown as f64 / 1e6,
        log.size() as f64 / 1e6,
    );
}

fn main() {
    let sizes: Vec<usize> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let sizes = if sizes.is_empty() {
        vec![10_000, 100_000]
    } else {
        sizes
    };
    let key = DeviceKey::from_seed([1; 32]);
    let other = DeviceKey::from_seed([2; 32]);
    // Next to the benchmark, not in /tmp, which may be memory.
    let base = std::env::current_dir().unwrap().join("target");
    std::fs::create_dir_all(&base).unwrap();
    for size in sizes {
        for run in [today, redb] {
            let dir = tempfile::tempdir_in(&base).unwrap();
            run(dir.path(), &key, &other, size);
        }
        let dir = tempfile::tempdir_in(&base).unwrap();
        amortised::<RedbLog>(dir.path(), &key, &other, size);
        let dir = tempfile::tempdir_in(&base).unwrap();
        amortised::<SqliteLog>(dir.path(), &key, &other, size);
    }
}
