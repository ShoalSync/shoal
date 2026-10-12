# 0037: Disk space and I/O errors

- Status: accepted
- Date: 2026-09-27

## Context

- **Missing folder roots.** When a folder opens, the engine creates its directory if it is
  missing (`open_folder` calls `create_dir_all`), and nothing checks that the directory is the
  folder it was. If a folder lives on a USB drive or SD card that is not mounted, the engine
  creates an empty directory at the mount point, and the next scan finds every file gone. With
  `local deletes = everywhere`, the default outside phone folders, that sends a delete for every
  file to every device. The files survive only in recycle bins.
- **Full disks.** Nothing stops downloading when a disk fills. Kept files and version history
  (ADR 0027, ADR 0029) also take space, bounded only by retention.
- **Failed writes.** A failed metadata save after remote changes is logged as a warning and
  syncing carries on (`backend.rs`), so what is on disk and what peers were told can diverge. A
  file that fails to write is a warning per pass, retried forever.

## Decision

### Folder roots

- Each folder's root holds a marker, `.tidefell/folder`, with the folder ID and this device's ID.
  It is written when the folder is created or joined.
- The engine creates a folder's root only when the folder is created or joined. At any other
  time, a root that is missing, or lacks a matching marker, puts the folder in an `unavailable`
  state: no scans, no writes, no deletes, and its files are still served from other devices. It
  checks again every minute and when the watcher sees the path appear, and resumes when the
  root is back.
- **Mass deletes pause.** If one scan would record deletes for more than half of a folder's
  files, and at least 100 of them, it records none and asks for confirmation in the app, the web
  and the CLI. A folder can set a different threshold, or none.

### Disk space

- Each volume the engine writes to (every folder's root, `metadata_dir`, `data_dir`) has a
  floor of free space: 2% of the volume or 1 GiB, whichever is smaller, set with
  `min_free_space`. Below it, downloads to that volume pause; local changes are still recorded
  and sent, and serving continues. Downloads resume when free space is back above the floor by
  10%.
- A file is written only if its size fits above the floor. Otherwise it waits, listed as a
  problem (ADR 0030).
- The status, the app and the web say which volume is low and how much the recycle bin and
  history hold there, with a way to empty them.

### Errors

- A metadata write that fails stops that folder from applying remote changes until it succeeds.
  It never records index progress it has not saved, so peers send the changes again. The folder
  is in an `error` state with the reason, and retries with backoff.
- File errors are classified. Transient ones (no space, interrupted) retry when the cause
  clears; others (permission denied, read-only filesystem, I/O error, name refused) become a
  problem and are retried only when the file or folder changes, or on request.
- A volume that becomes read-only or disappears makes the folder `unavailable`, as above,
  rather than a stream of per-file errors.

## Implementation notes

- Mass deletes (Phase 5k step 2): a scan counts the deletes it would send to every device (a
  folder with `local_deletes = this-device` sends none) against the files recorded on disk
  there. Held, the files stay recorded as on disk, so nothing is fetched back meanwhile, and the
  scan records no delete at all. The decision is `tidefell folder deletes <id> confirm|restore`,
  `POST /v1/folders/{id}/deletes`, the web folder page or the app: confirm runs a scan that
  records them; restore forgets the files were on disk, so they are fetched from other devices.
  Putting the files back clears the hold at the next full scan. The threshold is
  `confirm_deletes_over`, a percentage, in the configuration file; a folder added at runtime
  uses 50.

- Failed metadata writes (step 3): while a folder's commits fail, it applies no versions from
  peers and records no peer's progress, so peers send them again once saving works; changes
  made here are queued in order meanwhile. The folder is in the `error` state with the reason,
  and commits are retried after 2 seconds, doubling to a minute.

- Free-space floors (step 3): before writing a file, the engine checks that its size fits
  above the floor of the folder's drive and of `data_dir`'s, where its chunks arrive; one that
  does not waits as a `disk full` problem. A drive found below its floor takes nothing more
  until 10% above it. The folder's status (`low_space`) names the drive and how much the
  recycle bin and history hold, and the doctor fails a drive below its floor and warns below
  twice it, instead of the earlier fixed thresholds. `min_free_space` is in the configuration
  file and `Engine::set_min_free_space`.

- Classified errors (step 3): permission denied, a read-only filesystem, the drive's own I/O
  error (EIO) and a refused name last; any other error, such as an interrupted call or one
  from the engine, is retried every pass. A write or delete that fails in a lasting way is not
  tried again, its problem carried without counting
  attempts, until the content to write changes, a scan covers the path (a full scan, or the
  watcher's, which also sees permission changes) or `retry` is asked for (API, CLI, FFI). Disk
  full, unavailable content and files changing while read are retried every pass. Not done
  yet: a drive that turns read-only still gives a problem per file rather than making the
  folder unavailable.

## Consequences

- Unplugging a drive, or starting before it mounts, no longer deletes a folder everywhere.
- A full disk pauses downloads visibly instead of failing file by file, and never corrupts
  metadata.
- A deliberate mass delete of more than half a folder needs one confirmation.
- Folders created before the marker existed get it on first start after the upgrade, if their
  root holds files the metadata knows about.
