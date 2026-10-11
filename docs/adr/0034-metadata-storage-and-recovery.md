# 0034: Metadata storage and recovery

- Status: accepted
- Date: 2026-09-27

## Context

Each folder's metadata lives in two files in `<data_dir>/folders/<id>/`: `meta` (versions, the
recycle bin, the index epoch) and `state` (manifests, the chunk index, files on disk, peer
progress, confirmed copies, kept files and history).

- **Every save rewrites everything.** A save serialises the whole folder into a temporary file,
  fsyncs it and renames it into place. It runs at 21 call sites, including once per index batch
  received and once per pass that writes files, and it runs while holding the folder's lock,
  which FUSE, the app and the API wait on. At the 100,000-entry target the index alone is about
  36 MB, so a first sync that arrives in about 200 batches rewrites several GB, and each later
  change rewrites the whole folder again.
- **Renames are not made durable.** Neither save fsyncs the directory after renaming, so a power
  cut can lose the rename.
- **One location for everything.** `data_dir` holds the device key, the certificate, the block
  store, the folders' metadata and the logs. On a NAS whose data sits on large, slow disks, the
  metadata ends up there too.
- **Loss is not handled.** A `state` file that fails to decode returns `CorruptState`, and
  `Engine::start` opens every folder with `?`, so one corrupt folder stops `tidefelld` for all of
  them. There is no integrity check, rebuild or repair command. A missing `state` file partly
  heals: the next scan rematches files by content and peers resend the index. But the recycle
  bin, version history, kept files (ADR 0027) and confirmed copies are lost, and kept files are
  left orphaned in `.tidefell/deleted/`.

## Decision

### Storage

- Each folder's metadata moves to its own database, a single file per folder using `redb`, an
  embedded, transactional, crash-safe key-value store in pure Rust. Its tables hold what `meta`
  and `state` hold today: versions and conflict copies, the recycle bin, manifests, the chunk
  index, files on disk, peer progress, confirmed copies, kept files and history.
- A change writes only the records it touches, in one transaction. A local change, which signs
  a version, commits durably before the version is sent to peers. An index batch commits
  durably once applied, before its `up_to` is recorded, so an unsaved batch is simply sent again.
  Other changes, such as last-used times, commit without waiting for the disk.
- The folder lock is no longer held across a disk write; readers use the database's snapshots.
- One database per folder, so damage to one folder never touches the others.
- The implementation pull request benchmarks `redb` against SQLite at 1,000,000 entries before
  committing to it, and records the result here.
- Wherever the engine renames a file into place it also fsyncs the directory.

### Location

- `tidefelld` gains `metadata_dir`, also available through the CLI and the API. It holds the
  folder databases and nothing else, and defaults to `<data_dir>/metadata`. It is meant for fast
  storage, such as an SSD, when the folders and `data_dir` are on slow disks.
- The device key, the certificate, the block store, the runtime settings and the logs stay in
  `data_dir`.
- Each database records the device ID and the folder ID. On start, if `metadata_dir` has no
  database for a folder but the previous location does, `tidefelld` refuses to start and says to
  move the directory, instead of silently rebuilding.
- Android keeps its metadata in app storage, with no setting.

### Detection and rebuild

- On open, and daily, the engine checks each folder's database (the database's own checksums
  and an integrity check). `tidefell doctor` runs the same check.
- A folder whose database is missing, fails to open or fails the check does not stop the others.
  Its database is moved aside as `<id>.corrupt-<time>`, and the folder is rebuilt:
  1. It takes a new index epoch, so every peer resends its full index.
  2. It scans the folder: files on disk give their manifests and are matched to entries by
     content as versions arrive.
  3. Kept files are re-registered from sidecars. Each file kept for the recycle bin or history
     (ADR 0027, ADR 0029) gets a small sidecar next to it, `<name>.json`, holding its entry, path,
     version and time kept, so a rebuild restores the recycle bin and history exactly.
  4. Confirmed copies are learnt again from full replicas. A counter this device used before the
     loss and uses again after it is safe: ADR 0024 treats equal vectors with different content
     as concurrent.
  5. Per-device settings (sync options, retention, pins) live in the runtime settings in
     `data_dir`, not in the database, so they survive.
- Until the rebuild finishes, the folder is in a `rebuilding` state: it serves nothing it has
  not re-verified, and it records no deletes, so a slow rescan never looks like files removed.
- The app, the web and `tidefell status` say when and why a folder was rebuilt. `tidefell folder
  rebuild <id>` forces one.
- Existing `meta` and `state` files are converted on first start and then deleted; a folder whose
  files do not convert is rebuilt.

### Steps

- Step 2 measured (`bench/metadata`, `cargo run --release` there), applying one batch of 512
  versions from another device to a folder of signed versions, on the CI machine's disk
  (ZFS). Times include applying the batch in memory and checking its signatures.

  | Entries | Today: rewrite `meta` | redb, by entry | SQLite, by entry |
  | --- | --- | --- | --- |
  | 10,000 | 61 ms, 2.9 MB | 146 ms, 2.2 MB | 273 ms, 3.6 MB |
  | 100,000 | 388 ms, 29 MB | 100 ms, 3.0 MB | 192 ms, 14.7 MB |
  | 1,000,000 | 2,841 ms, 290 MB | 129 ms, 3.9 MB | 205 ms, 15.0 MB |

  Today's figure is the `meta` file alone; the engine's `state` file is written on top. Keyed
  by entry, every database rewrites a page for each of the 512 entries it touches, about 3 MB.
  Keyed by the sequence this device stored each version under, a batch appends: redb writes
  0.33 MB per batch at any size. Superseded versions are removed later, in one sorted pass once
  there are as many as live ones. Over 400 batches at 100,000 entries, clean-ups included:

  | Engine | Batch, mean / slowest | Written per batch | File: loaded / grown / compacted |
  | --- | --- | --- | --- |
  | redb | 58 / 82 ms | 0.47 MB | 67 / 270 / 65 MB |
  | SQLite | 59 / 97 ms | 0.76 MB | 63 / 127 / 70 MB |

- Step 2 decided:
  - redb, as decided above: it writes less, is pure Rust, and needs no C toolchain for the
    Android build. Versions are kept by sequence; where each entry's versions are is kept in
    memory and rebuilt on open; superseded versions are removed in one sorted pass once there
    are as many as live ones, and the database is then compacted, since redb does not give
    freed pages back to the file system on its own. A folder's database takes about twice the
    space of today's `meta` file.
  - Signatures are checked before the folder is locked, so the lock covers applying the batch
    in memory; the commit happens outside it, and peers are sent only committed versions.

- Step 3 decided: every rename into place in the engine, `tidefell-meta`'s snapshot and the web
  peer's accounts and shares files syncs the directories it touches afterwards. A filesystem
  that cannot sync a directory, as some FUSE mounts cannot, keeps the rename without the
  guarantee rather than failing it. The block store's chunk files do not: a lost chunk is fetched
  again, and a directory sync per chunk would cost transfers more than it saves. Transfer times
  of Phase 5h's measurement are unchanged.
- Step 4 decided:
  - `tidefell-meta` keeps a journal of what each change stored, dropped and recycled, and
    rebuilds a folder from what was written, choosing each entry's current version as storing
    does, so nothing per entry is rewritten. Versions are not verified again on open; they were
    when they arrived, and the database's checksums cover what was written since. Checking a
    remote version is separate from storing it, so it can be done before the folder is locked.
  - Each folder's database is `folder.redb` in its state directory, with a header table, the
    versions by sequence, an append-only table of dropped sequences, the recycle bin, and one
    table for each of the engine's records. Those records sit in maps that note which keys
    change, so a change the engine makes without saving it does not compile; a development
    build also reads the whole database back after each save, for folders of up to 2,000
    entries, and fails if it differs from memory.
  - A save commits only what changed, durably. The `meta` and `state` files of earlier
    versions are converted when a folder opens and then deleted.
  - Closing a folder's database is part of shutting the engine down, since redb locks the
    file: Android restarts its engine in the same process.
  - A save takes what changed while the folder is locked and queues it; a thread of the folder's
    own commits the queue in order, retrying a commit that fails, then writes the activity log.
    Peers are sent versions only up to the last sequence committed, so a local change or a
    received batch is on disk before any peer hears of it, and an index response never names
    progress beyond it. Last-use times alone are committed without waiting for the disk.
  - Remote versions are verified before the folder is locked. With 100,000 entries, a batch of
    512 holds the lock for at most 3.4 ms and writes 0.55 MB, clean-ups included
    (`a_batch_at_100000_entries`). A daemon killed twice during a first sync of 1,500 files
    completes it after restarting, with every file intact and nothing doubled
    (`killed_during_a_first_sync_loses_nothing`).

- Step 5 decided:
  - Each folder's database is `<metadata_dir>/<folder id>.redb`. The activity log stays in
    `<data_dir>/folders/<id>/` with what else is not metadata.
  - `data_dir` records the metadata directory in use, in `metadata-dir`. If `metadata_dir` is
    set elsewhere and a folder has a database in the recorded place but none in the new one,
    `tidefelld` does not start, and says where the databases are and where they are expected.
  - `tidefell status` and the API's status show the directory; it is set only in the
    configuration file.
- Step 6 decided, so far:
  - Sidecars are written when a file is kept, removed with it, and written on open for files
    that have none, as those kept by earlier versions do not. A kept file's sidecar lists the
    recycle-bin items with its content, and is rewritten when the recycle bin changes; a
    history file's holds its entry and record; a set-aside file's its entry and record.
    Records holding version vectors, which JSON cannot key by device, are kept exactly as
    their postcard encoding in hex beside a readable path. A sidecar that cannot be written
    costs only what a rebuild could restore, so it never fails the operation.

## Consequences

- Saving costs roughly the size of the change, not of the folder, and never blocks readers,
  which makes the 100,000-entry target and larger folders practical.
- A NAS can keep its metadata on an SSD while its data stays on slow disks.
- Losing or damaging metadata costs a rebuild of one folder, not the daemon, and keeps the
  recycle bin and history through their sidecars.
- `redb` (or SQLite) becomes a dependency of the engine and must pass `cargo-deny`.
- Kept files gain a sidecar each.
