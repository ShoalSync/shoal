# Blob transfer protocol, version 2: encrypted folders

ALPN: `shoal/blobs/2`

Status: draft. Messages are to be implemented in `crates/proto`; golden encodings are in
[`vectors/blobs-v2.json`](vectors/blobs-v2.json), made by
[`vectors/gen/blobs_v2.py`](vectors/gen/blobs_v2.py) from this text. See ADR 0023, as amended.

## Purpose

Transfer the sealed chunks and list blobs of encrypted folders ([encryption-v1.md](encryption-v1.md))
between member devices and storage peers ([sync-v2.md](sync-v2.md)), and tell peers which of them
a replica holds. It runs alongside blobs/1, which plaintext folders keep using; everything
blobs/1 says holds here except where this page says otherwise.

## Blobs

A **blob ID** is 32 bytes: a chunk ID, naming a sealed chunk, or a list ID, naming a list blob.
The two cannot be told apart from the ID; the requester knows which it asked for.

- A **list blob** is checked by anyone: its BLAKE3 hash must equal its list ID.
- A **sealed chunk** is checked only by a member holding the key: it opens it as
  encryption-v1.md says. A storage peer cannot check the sealed chunks it fetches and stores
  them as received. A member that finds a sealed chunk wrong treats it as missing from that peer
  and fetches it from another.

## Access control

As blobs/1, with sync/2's devices: a member device serves the devices of the folder's member
users and its storage peers; a storage peer serves the devices of the member users its operator
listed for the folder. A replica serves only blobs referenced by a version it holds of that folder,
current or kept for history, or by a list blob such a version names.

## Messages

| Index | Message | Fields |
| --- | --- | --- |
| 0 | `Hello` | `certificate: Signed<DeviceCertificate>` |
| 1 | `Request` | `folder: FolderId`, `blobs: Vec<BlobId>` (at most 64) |
| 2 | `Blob` | `id: BlobId`, `data: bytes` |
| 3 | `Missing` | `id: BlobId` |
| 4 | `AvailabilityRequest` | `folder: FolderId`, `blobs: Vec<BlobId>` (at most 4096) |
| 5 | `Availability` | `bitmap: bytes` |
| 6 | `Error` | `code: BlobsErrorCode` |
| 7 | `ReplicaRequest` | `folder: FolderId` |
| 8 | `Replica` | `full: bool` |
| 9 | `Busy` | `retry_after_ms: u32` |

`BlobsErrorCode` variants: `0 NotAMember`, `1 UnknownFolder`, `2 TooManyBlobs`,
`3 ProtocolViolation`.

A `Blob`'s `data` is at most 1,052,688 bytes for a sealed chunk (1 MiB of plaintext, 4 KiB of
padding and the 16-byte tag), and at most 131,074 bytes for a list blob (4096 chunk IDs and their
length prefix).

## Exchanges

**Blobs.** As blobs/1's chunks: for each requested blob, in request order, `Blob` or `Missing`,
or a single `Error`; `Busy` as in blobs/1. The requester checks each blob as above, where it can,
and discards one that fails.

**Availability.** The requester names up to 4096 blobs; `Availability`'s `bitmap` has one bit
per named blob, in order, as in blobs/1 (`ceil(n / 8)` bytes, least significant bit first), set
if the responder holds it. A member asks about a file's chunk IDs to learn which peers hold its
content. A full replica that holds every chunk of a file is a confirmed copy of it (ADR 0022),
except that a storage peer counts only while a member device other than the one asking holds a
grant of the key the file is sealed with: otherwise the copy could not be read if the asking
device were lost.

**Replica kind.** As blobs/1. A storage peer is a full replica: it keeps every blob of every
version it holds.
