# Storage protocol, version 1: what a storage peer keeps

ALPN: `shoal/storage/1`

Status: draft. Messages are implemented in `crates/proto`; golden encodings are in
[`vectors/storage-v1.json`](vectors/storage-v1.json), made by
[`vectors/gen/storage_v1.py`](vectors/gen/storage_v1.py) from this text. See ADR 0023's
amendment of 2026-10-22.

## Purpose

Let the member devices of an encrypted folder ([sync-v2.md](sync-v2.md)) tell a storage peer how
to rank what it keeps of the folder: a priority for each entry. A storage peer cannot read paths,
so members, who can, turn their rules into priorities by entry ID.

It is a protocol of its own, beside sync/2 and blobs/2, rather than new sync/2 items: sync/2 has
no way to skip an item a peer does not know, and storage peers are run by other people, so they
may lag their members' versions. A device that does not speak it refuses the ALPN.

## Priorities

A **priority** is a number from 0 to 255; higher is kept first. An entry no member has ranked is
128. 0 means "never store": a storage peer does not fetch an entry ranked 0, and so does not hold
it or count as a copy of it.

A storage peer keeps, for each entry it holds a version of, the priority it was last given, with
the time the member device gave it (`issued_ms`, by that device's clock). It replaces a priority
only with one issued later, and forgets an entry's priority once it keeps no version of it.

## Access control

As blobs/2: a storage peer accepts streams from the devices of the member users its operator
listed for the folder, vouched for as sync/2 says. A device that stores no such folder, or is
not a storage peer of it, answers `Error` with `NotStored`.

## Messages

Each message is a frame: a varint length, then the message's postcard encoding.

| Index | Message | Fields |
| --- | --- | --- |
| 0 | `Hello` | `certificate: Signed<DeviceCertificate>` |
| 1 | `Priorities` | `folder: FolderId`, `issued_ms: i64`, `hints: Vec<Hint>` (at most 4096) |
| 2 | `Accepted` | |
| 3 | `Error` | `code: StorageErrorCode` |
| 4 | `UsageRequest` | `folder: FolderId` |
| 5 | `Usage` | `quota_bytes: u64`, `held_bytes: u64`, `room_bytes: u64` |
| 6 | `Drop` | `folder: FolderId`, `below: u8` |
| 7 | `Dropped` | `chunks: u64`, `bytes: u64` |

`Hint` is `entry: EntryId`, `priority: u8` (one byte).

`StorageErrorCode` variants: `0 NotAMember`, `1 NotStored`, `2 TooManyHints`,
`3 ProtocolViolation`.

## Exchanges

Each request is a stream of its own. The requester sends `Hello`, then its request; the
responder checks the certificate, as blobs/2 does, and answers once.

**Priorities.** A member device sends the priorities of the folder's current entries, all of
them, in `Priorities` messages of up to 4096 hints, each on a stream of its own and all with the
same `issued_ms`. The storage peer answers `Accepted` once it has recorded them, or `Error`. A
member sends them again when the rules or the entries change, so an entry a rule no longer
covers goes back to 128.

**Usage.** The storage peer answers `Usage`: the folder's quota, the sealed bytes it holds of it,
and the room it has left for it, under both the folder's quota and its total across folders. It
learns a chunk's size only by fetching it, so it says nothing of what it lacks: a member works
that out, by asking blobs/2's availability of the chunk IDs of its current versions, whose
lengths it knows.

**Drop.** A member asks the storage peer to forget what it holds of the entries ranked below
`below`, for room for those ranked higher: the chunks no kept version of an entry ranked at
`below` or above lists. It answers `Dropped`, how many chunks and sealed bytes it removed. It does
so only when asked: never to make room on its own. A member asks only when a user does, and
checks first that each file it would drop has a copy elsewhere (ADR 0023's amendment of
2026-10-22). What it dropped it fetches again, highest priority first, when it has room.

## What a storage peer learns (informative)

Besides what sync/2 and blobs/2 show it: which entries the members rank above or below others,
and which they keep from it. Not why: paths and names stay sealed.
