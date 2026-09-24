# Blob transfer protocol, version 1

ALPN: `tidefell/blobs/1`

Status: draft. Messages are implemented in `crates/proto` (`blobs` module); golden encodings
are in [`vectors/blobs-v1.json`](vectors/blobs-v1.json). See ADR 0008 for why this is not
iroh-blobs.

## Purpose

Transfer chunk bytes between peers, and tell peers which chunks of a file's content a replica
holds so they can fetch from several peers at once (rarest first).

## Transport and encoding

A QUIC connection negotiated with ALPN `tidefell/blobs/1` over iroh. Framing and encoding are
the same as `tidefell/sync/1`: varint length prefix, postcard body, 16 MiB maximum frame.

The requester opens a bidirectional stream per request and sends `Hello` followed by one
request. The responder answers and finishes the stream. A requester may use many streams on one
connection concurrently.

## Access control

The responder checks `Hello.certificate` as in `sync/1` (signed by its user, `device` equals the
connection's endpoint ID) and that the certificate's user is a member of the requested folder.
It only serves chunks and availability for content that is referenced by a current version or
recycle-bin entry of that folder, so membership of one folder never reveals another folder's
data.

## Messages

| Index | Message | Fields |
| --- | --- | --- |
| 0 | `Hello` | `certificate: Signed<DeviceCertificate>` |
| 1 | `Request` | `folder: FolderId`, `chunks: Vec<ChunkHash>` (at most 64) |
| 2 | `Chunk` | `hash: ChunkHash`, `data: bytes` |
| 3 | `Missing` | `hash: ChunkHash` |
| 4 | `AvailabilityRequest` | `folder: FolderId`, `content: ContentHash` |
| 5 | `Availability` | `content: ContentHash`, `chunk_count: u32`, `bitmap: bytes` |
| 6 | `Error` | `code: BlobsErrorCode` |
| 7 | `ReplicaRequest` | `folder: FolderId` |
| 8 | `Replica` | `full: bool` |

`BlobsErrorCode` variants: `0 NotAMember`, `1 UnknownFolder`, `2 TooManyChunks`,
`3 ProtocolViolation`.

## Exchanges

**Chunks.** Requester: `Hello`, `Request`. Responder: for each requested hash, in request order,
either `Chunk` or `Missing`; or a single `Error`. The requester hashes each `Chunk.data` with
BLAKE3 and discards it, and treats the peer as faulty, if it does not equal `hash`.

**Availability.** Requester: `Hello`, `AvailabilityRequest`. Responder: `Availability` or
`Error`. `bitmap` has `ceil(chunk_count / 8)` bytes; bit `i` of byte `i / 8` (least significant
bit first) is set if the responder holds chunk `i` of the content's manifest. A responder that
does not know the content replies `Availability` with `chunk_count = 0`.

**Replica kind.** Requester: `Hello`, `ReplicaRequest`. Responder: `Replica` or `Error`.
`full` is true if the responder is a full replica of the folder, keeping every file, and false
if it is a partial replica that may evict files. Peers use it to decide which copies count as
confirmed (ADR 0022). `ReplicaRequest` and `Replica` were added to blobs/1 after its first
release: a responder from before then cannot decode the request and closes the stream without
replying. A requester treats any answer other than `Replica` as a partial replica.

## Open questions

- Pushing availability changes instead of polling.
- Per-peer rate limiting.
