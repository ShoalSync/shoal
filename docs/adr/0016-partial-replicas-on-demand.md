# 0016: Partial replicas keep placeholders and fetch on demand

- Status: accepted
- Date: 2026-09-23

## Context

Until now a partial replica stored metadata only. Phones (and later the Linux FUSE filesystem)
need to show every file in a folder while storing only some of them, fetch a file when it is
opened, and stay within a storage budget.

## Decision

- In a partial replica every live file is a **placeholder** until its content is requested.
  Scans never turn a missing placeholder into a delete; only a file the engine materialised and
  that then disappears is a local delete.
- The engine gains `open(folder, path)`: it fetches missing chunks from connected peers (swarm
  fetch), assembles and verifies the file into the folder's directory, marks it materialised,
  and returns its path. It also gains `write(folder, path, source)`, which moves new content
  into place and records the change without waiting for the next scan.
- A partial replica's byte budget counts its materialised files. Opened files are held while
  in use. After each fetch and on a timer, the engine evicts the least recently used files not
  held open until the folder fits its budget: an evicted file is removed from disk, if unchanged,
  becomes a placeholder again, and its chunks leave the block store unless another folder still
  needs them.
- A file whose current version was made on this device is evicted only once a connected peer
  reports holding all of its content, so eviction never loses the only copy.
- On Android the folder directory is in app-private storage, and a `DocumentsProvider` exposes
  folders to the system file picker and the Files app: listings come from metadata, opening a
  document calls `open`, and saving calls `write`. The phone does not scan shared storage.

## Consequences

- The same engine support serves Android now and `fs-fuse` later.
- Opening a large placeholder while offline fails with a clear error instead of blocking.
- Camera-roll backup (Phase 5) is a separate, upload-only feature and does not depend on this.
