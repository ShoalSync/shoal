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

## Consequences

- Saving costs roughly the size of the change, not of the folder, and never blocks readers,
  which makes the 100,000-entry target and larger folders practical.
- A NAS can keep its metadata on an SSD while its data stays on slow disks.
- Losing or damaging metadata costs a rebuild of one folder, not the daemon, and keeps the
  recycle bin and history through their sidecars.
- `redb` (or SQLite) becomes a dependency of the engine and must pass `cargo-deny`.
- Kept files gain a sidecar each.
