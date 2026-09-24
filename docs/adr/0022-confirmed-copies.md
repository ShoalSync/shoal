# 0022: Confirmed copies

- Status: accepted
- Date: 2026-09-24

## Context

Some decisions need to know that a file is safe somewhere else:

- **Eviction.** A partial replica evicts a file it made only once a connected peer reports
  holding all of its content (ADR 0016). It asks at that moment, and any peer counts,
  including another partial replica that may evict the file in turn.
- **Removing originals after camera-roll backup** (ADR 0020) must wait until "a peer confirms
  it holds the content". It was deferred from Phase 5 because the engine had no way to say so.
- **People** want to see whether a file is backed up before they wipe a phone or delete a local
  copy.

The engine already asks peers which chunks they hold, with blobs/1 `AvailabilityRequest`, but
it keeps no record of the answers and cannot tell a full replica from a partial one.

## Decision

- **A confirmed copy** of a file's content is a full replica, other than this device, that
  has reported holding every chunk of it. Partial replicas do not count, because they may evict
  a file that another device made at any time.
- **Replica kind on the wire.** blobs/1 gains a message pair, appended after the existing
  messages: `ReplicaRequest { folder }` and `Replica { full: bool }`. A peer that does not
  know them replies with `Error(ProtocolViolation)` and is treated as partial. Appending a
  message is compatible with blobs/1; the spec and golden vectors are updated.
- **The engine records confirmations.** For each folder it keeps, per content hash, the
  full replicas that confirmed it and when, persisted with the folder's state. It checks only
  content that this device made and that has no confirmed copy yet. It checks when a peer
  sharing the folder connects, after local changes have been announced, and every few
  minutes while there are unconfirmed files and connected full replicas. It stops checking a
  file once it has a confirmed copy. Confirmations are dropped when the content is no longer
  referenced by a current version or the recycle bin.
- **Eviction uses the same rule.** A partial replica evicts a file it made only if it has a
  confirmed copy. A file made elsewhere may still be evicted freely, as today.
- **Surfaces.** The engine exposes the confirmed copies of a file (device, name, time). The
  API, sdk and `ffi` report per file whether it has a confirmed copy, plus the details on
  request. `tidefell ls` and the web peer mark files that exist only on this device, and
  `tidefell status` counts them per folder.
- **Removing originals after backup** (ADR 0020) becomes possible on Android 11 (API 30) and
  later. The setting is off by default and asks for confirmation when turned on. It applies only
  to items backed up after it was turned on. Once an item's copy in the folder has a
  confirmed copy, the item is eligible. The app groups eligible items and deletes them through
  the system's delete request, which Android shows as its own confirmation dialog. A
  notification opens it, because background work cannot show the dialog.
- **A confirmation is a point in time.** A full replica that later loses its disk or leaves
  the folder no longer holds the file. The docs say so. Re-checking copies that were already
  confirmed is out of scope.

## Consequences

- A folder whose only replicas are partial can no longer evict files made on this device,
  so it can grow past its budget. The Android app and `tidefell status` say when that
  happens, and suggest an always-on full replica (ADR 0018).
- Availability checks add some traffic. They cover only unconfirmed files made on this device,
  and each file stops being checked once it has a confirmed copy.
- blobs/1 gains two messages and their vectors. Older peers keep working and never count as
  confirmed copies.
