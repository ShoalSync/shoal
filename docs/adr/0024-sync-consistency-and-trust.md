# 0024: Sync consistency and trust

- Status: accepted
- Date: 2026-09-25

## Context

A review of the codebase found cases where sync/1, as specified or as built, loses data,
never converges, stalls or trusts too much:

- **Lost edits.** A device building a new version merges the vectors of every version it
  knows of the entry, including a remote version that arrived but was never written to disk
  because the file had local changes. The new version then supersedes the remote one, which
  disappears without a conflict copy. The spec prescribes this ("the merge of the entry's
  current version and any conflict copies").
- **Same path, different entries.** Two devices that create `notes.txt` while apart make two
  entries with different IDs. Conflicts are resolved per entry, so both stay live at one path
  and replicas disagree about which one is on disk. A file and a directory can also claim the
  same path (`a` and `a/b`).
- **Equal vectors.** A device restored from a backup, or two devices sharing a key, can sign
  two different versions with equal vectors. Each replica keeps the first it sees, forever.
- **Counters.** Incrementing a counter at `u64::MAX` panics or wraps. Any member can send
  such a counter for another device.
- **Unbounded exchange.** A responder writes its whole index before reading again, and both
  sides push back what they just received. Past roughly 20,000 versions both peers block
  writing, and the connection stays up, so the stall is permanent. Requests have no deadline,
  so one silent peer can hold a folder's file operations.
- **Unchecked inputs.** A manifest is cached if its hash matches, even if its chunks do not add
  up to the file's size, and it is then used first on every attempt. `.tidefell`, the reserved
  per-folder directory, is refused only by the web peer.
- **Authors.** A version must be signed by its author, but nothing requires the author to be a
  device of a folder member. Removing a member cannot stop that member's devices, and any
  member can mint keys.
- **Confirmed copies** (ADR 0022) count any peer that says it is a full replica.

## Decision

### Threat model

Members of a folder are trusted with its contents: they can already delete or overwrite every
file. The protocol must:

- never let a non-member read or change a folder;
- keep every change attributable to a device certified by a member;
- make sure no input from any peer, member or not, crashes a replica, corrupts its state,
  affects other folders or blocks it indefinitely.

A misbehaving member may still disrupt a folder they belong to. The remedy is removing them.

### Versions and conflicts

- **A local change builds on what is on disk.** Each replica records, for every file it has
  written to or scanned from disk, the version vector that content corresponds to: its base.
  A local change sets the new vector to the merge of its base and the conflict copies that
  existed when that content was written, then increments its own counter. A remote version
  that arrived but was never written, because the file had local changes, stays concurrent
  with the new version and becomes a conflict copy. (ADR 0029 changes this: a change no
  longer merges conflict copies with other content, and vectors can have holes, so conflicts
  last until they are resolved.)
- **Equal vectors with different content are concurrent.** Two versions of an entry whose
  vectors are equal but whose encodings differ are resolved by the existing rules; rule 4
  (the greater encoding wins) decides. Identical encodings are the same version.
- **One entry per path, decided when writing to disk.** Among live entries whose paths are
  equal, the one with the lower `EntryId` (compared as bytes) keeps the path. Every other one
  is written as `<stem> (conflict <id>)<extension>`, where `<id>` is the first 8 hex digits of
  its `EntryId` (all 32 if that name is taken too). A file or symlink whose path is also a
  directory, explicit or implied by entries below it, gives way to the directory. Two
  directories at one path are one directory. This rule adds no versions: every replica
  computes the same result from the same entries. The first local change to a displaced
  entry, including a rename, records its current name as its path.
- **Counters have a ceiling.** A counter above 2^62 makes a version invalid; it is refused
  like a malformed message. A device refuses to make a change that would pass the ceiling.
- **Restoring a device's data from a backup, or copying it to another device, is not
  supported.** A device must re-pair instead. The Android app excludes its data from backups
  and device transfer.

### Inputs

- **Reserved path.** A path whose first component is `.tidefell` is invalid, like `..`.
- **Manifests.** A manifest is accepted only if its chunks hash to its content and their
  lengths add up to the `size` of the version that asked for it. A manifest that fails
  assembly is dropped from the cache and fetched again.
- **Sizes before authentication.** A peer reads at most 64 KiB for a message before the
  handshake has completed.

### Flow control and deadlines

- **Index pages.** An `IndexBatch` carries at most 512 versions. A responder writes batches from
  a task separate from the one that reads, so reading never waits on writing.
- **No echo.** A peer does not push a version back to the peer it received it from.
- **Deadlines.** A handshake must complete within 10 seconds. A requester gives up on a
  request that gets no response within 30 seconds and treats it as failed; a stream that
  delivers no data for 30 seconds during a transfer is failed too. File operations never wait
  on the network while holding a folder's lock.

### Authors

- **An author must be vouched for.** A version is applied only if its author has a device
  certificate signed by a user who is a member of the folder. Certificates travel on a new
  request stream in sync/1: `CertificateRequest { folder, device }`, answered by
  `CertificateResponse { certificate }` or `Error`. A replica keeps the certificates of the
  authors of the versions it stores and answers for any of them.
- **Pending versions.** A version whose author has no known certificate is held, not applied
  or relayed, and requested again later.
- **Removed members.** When a user is removed from a folder, versions later received from
  that user's devices are refused. Versions already applied stay.
- **Rollout.** Replicas record whether each author is vouched for from the release that adds
  the certificate stream, and enforce the rule from the release after it. That gives every
  peer time to serve certificates.

### Confirmed copies

A confirmation counts only while the confirming device's user is a member of the folder, and
confirmations from a removed member's devices are dropped (ADR 0022 is otherwise unchanged).

## Consequences

- Concurrent edits made before a scan are kept as conflict copies instead of lost, and same-path
  entries resolve the same way everywhere, without new versions or rename storms.
- Replicas that do not yet follow these rules may resolve equal vectors and same-path entries
  differently until they are upgraded. Such peers are all devices of the same people, so they
  are expected to upgrade together.
- Large folders sync with bounded memory, and a silent peer costs at most a deadline.
- The certificate stream adds a message pair and a round trip the first time an author is
  seen. A version from an author whose certificate cannot be found waits until it can be.
- A member can still disrupt a folder they belong to. The damage is attributable, and removing
  them stops it for new changes.
- Restores from backup need re-pairing. That is simpler than detecting a rolled-back replica.
