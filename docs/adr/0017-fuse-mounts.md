# 0017: tidefelld mounts folders read-write with FUSE

- Status: accepted
- Date: 2026-09-24

## Context

ADR 0016 gave partial replicas placeholders and fetch on open, and Android exposes them through
a `DocumentsProvider`. On Linux the equivalent is a filesystem: every file of a folder is
visible, opening a placeholder fetches it, and the replica stays within its budget. Desktop
programs expect ordinary file semantics, including saving by writing a temporary file and
renaming it over the original.

## Decision

- **The daemon hosts the mount.** A folder in `tidefelld`'s config may set
  `mount = "/path"`. The daemon mounts the folder there when it starts or the folder is added,
  and unmounts it on shutdown or removal. The folder's `path` stays the engine's backing
  directory, which the user does not edit directly while it is mounted. Mounting is Linux-only;
  elsewhere a `mount` setting is rejected.
- **`fs-fuse` implements it with `fuser`** (MIT, no libfuse). Unprivileged mounts use
  `fusermount3`. The mount is private to the daemon's user.
- **Reads.** Listings and attributes come from the folder's metadata, so placeholders appear
  with their real size and modification time. Directories are implied by file paths. Opening
  a file calls the engine's `open`, which fetches a placeholder and pins it until the last
  handle closes; reads go to the materialised file. A placeholder that no connected peer can
  serve fails with `EIO`.
- **Writes.** Writing to a file copies it to a staging file first (unless it is truncated),
  and the new content is recorded with the engine's `write` when the last handle closes or on
  `fsync`. Creating a file works the same way. Renaming calls a new engine `rename`, which
  moves an entry, or every entry under a directory, to a new path. It keeps the content
  reference, so renaming a placeholder needs no fetch, and a rename over an existing file
  replaces it. Deleting calls a new engine `delete`, which records a delete that sends the
  file to peers' recycle bins. Empty directories created through the mount live in memory
  until a file is created in them, because empty directories are not synced.
- **Changes from peers** reach the mount through short attribute and entry lifetimes and
  kernel cache invalidation on engine events.
- **Not supported:** symlinks, hard links, extended attributes, file locks, and permission
  changes other than the executable bit. These fail with `ENOTSUP` or `EPERM`.

## Consequences

- A Linux partial replica behaves like a local directory that holds only what was opened
  recently. Eviction turns unopened files back into placeholders without the mount changing.
- `delete` and `rename` become engine operations. The API, the CLI and the ffi can expose them
  later without new engine work.
- CI mounts a real filesystem on the self-hosted runner, which needs `/dev/fuse` and
  `fusermount3`.
- A `tidefell mount` command that mounts from outside the daemon stays possible later on top of
  the API, but is not built now.
