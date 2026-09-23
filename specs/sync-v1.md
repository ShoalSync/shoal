# Sync protocol, version 1

ALPN: `tidefell/sync/1`

Status: draft. Messages are implemented in `crates/proto` (`sync` module); golden encodings are
in [`vectors/sync-v1.json`](vectors/sync-v1.json).

## Purpose

Exchange folder metadata between two devices: which folders they share, which entry versions
each is missing, and the manifests (chunk lists) of file content. Chunk bytes are transferred
separately over `tidefell/blobs/1`.

## Transport

A QUIC connection negotiated with ALPN `tidefell/sync/1` over iroh. Both sides authenticate by
their iroh endpoint key, which is the `DeviceId`.

- **Main stream.** The connecting peer opens the first bidirectional stream and sends `Hello`.
  `Hello`, `IndexRequest`, `IndexBatch` and `Error` travel on this stream for the life of the
  connection.
- **Manifest streams.** After the handshake, either peer may open further bidirectional
  streams, each carrying exactly one `ManifestRequest` and one `ManifestResponse` or `Error`,
  after which the responder finishes the stream. The handshake on the main stream
  authenticates the whole connection.

## Membership

Each folder has a set of member users. A device may sync a folder if it presents a device
certificate, signed by one of those users, whose `device` equals the connection's endpoint ID.

```
DeviceCertificate = user: UserId || device: DeviceId || name: string || issued_ms: i64
```

A certificate is sent as `Signed<DeviceCertificate>` with kind `identity/1/device-certificate`,
and its signer must equal `user`. Revocation is not yet specified.

## Encoding

Every message is one frame:

```
frame = varint(len) || body
body  = postcard(SyncMessage)
```

- `varint` is unsigned LEB128. Frames larger than 16 MiB are rejected.
- `body` uses the [postcard 1.x wire format](https://postcard.jamesmunns.com/wire-format):
  enums are a varint variant index followed by fields; integers are varints (signed integers
  zig-zag encoded); strings and `Vec`s are varint length then contents; fixed-size identifiers
  are raw bytes with no length prefix; `bool` is one byte.
- Field and variant order is fixed. Changing it requires `tidefell/sync/2`.

## Identifiers

| Type | Size | Meaning |
| --- | --- | --- |
| `DeviceId` | 32 | Device Ed25519 public key; also its iroh endpoint ID |
| `UserId` | 32 | User Ed25519 public key |
| `FolderId` | 32 | Random, chosen when a folder is created |
| `EntryId` | 16 | Random, chosen when an entry is created; stable across renames |
| `ChunkHash` | 32 | BLAKE3 of the chunk bytes |
| `ContentHash` | 32 | BLAKE3 of the full file content |

## Version vectors

`VersionVector` is a map `DeviceId → u64` encoded as a postcard map sorted by key, with no zero
counters. Version `a` supersedes `b` if every counter in `a` is ≥ the one in `b`, and at least
one is greater. If neither supersedes the other, they are concurrent (a conflict).

Version vectors are per entry and are used only to order versions of that entry. A device
making a change sets the new vector to the merge of the entry's current version and any
conflict copies, then increments its own counter.

## Index sequences

Sync progress is tracked separately from version vectors. Every replica keeps, per folder, an
index: each version it stores gets the next value of a local, monotonically increasing
sequence number when it is stored. The index also has an `epoch`, a random non-zero `u64`
chosen when the index is created; a new epoch means the sequence restarted and requesters must
fetch from the beginning.

A requester remembers, per peer and folder, the responder's `epoch` and the highest `up_to`
it has received, and sends them in its next `IndexRequest`.

## Signed envelopes

Attributable messages are sent in a signed envelope:

```
Signed<T> = signer: [u8; 32] || body: bytes || signature: [u8; 64]
signed bytes = "tidefell/sig/1\0" || u8(len(kind)) || kind || body
```

`body` is the postcard encoding of `T` and `signer` is an Ed25519 public key. Signatures are
verified in strict mode. Each kind states who must sign:

| Kind | Message | Signer must be |
| --- | --- | --- |
| `sync/1/entry-version` | `EntryVersion` | `EntryVersion.author` |
| `identity/1/device-certificate` | `DeviceCertificate` | `DeviceCertificate.user` |

## Messages

| Index | Message | Fields |
| --- | --- | --- |
| 0 | `Hello` | `certificate: Signed<DeviceCertificate>`, `folders: Vec<FolderId>` |
| 1 | `IndexRequest` | `folder: FolderId`, `epoch: u64`, `since: u64` |
| 2 | `IndexBatch` | `folder: FolderId`, `epoch: u64`, `up_to: u64`, `versions: Vec<Signed<EntryVersion>>`, `last: bool` |
| 3 | `ManifestRequest` | `folder: FolderId`, `content: ContentHash` |
| 4 | `ManifestResponse` | `manifest: Manifest` |
| 5 | `Error` | `code: ErrorCode` |

`EntryVersion`:

| Field | Type | Notes |
| --- | --- | --- |
| `folder` | `FolderId` | |
| `entry` | `EntryId` | |
| `path` | string | Relative, `/`-separated, NFC; non-empty components; no `.` or `..` components; no `\` or NUL |
| `kind` | `EntryKind` | See below |
| `modified_ms` | i64 | Milliseconds since Unix epoch, UTC |
| `author` | `DeviceId` | Must equal the envelope signer |
| `version` | `VersionVector` | Vector after this change |

`EntryKind` variants: `0 File { content: ContentHash, size: u64, executable: bool }`,
`1 Directory`, `2 Symlink { target: string }`, `3 Tombstone`.

`Manifest`: `content: ContentHash`, `chunks: Vec<ChunkRef { hash: ChunkHash, len: u32 }>`.
`content` must equal BLAKE3 of the concatenated chunks.

`ErrorCode` variants: `0 NotAMember`, `1 UnknownFolder`, `2 UnknownManifest`,
`3 ProtocolViolation`.

## Exchange

1. The connecting peer sends `Hello` listing the folders it syncs. The accepting peer checks
   the certificate (see Membership) and replies with `Hello` listing only the folders the
   connecting peer is a member of. Each side then proceeds only with folders that both sides
   listed and for which the peer is a member.
2. For each shared folder, each side sends `IndexRequest` with the epoch and sequence it last
   received from this peer (both 0 the first time).
3. The responder sends, in sequence order, every version currently in its index (current
   versions and conflict copies) whose sequence is greater than `since`, or all of them if
   `epoch` does not match its own. It uses one or more `IndexBatch` messages, the last with
   `last = true`. Superseded versions are not sent.
4. The requester verifies each version (signature, signer equals author, folder, path rules)
   and applies it. It records `epoch` and `up_to` only after applying the batch marked `last`.
5. A peer that needs a file's chunk list opens a manifest stream, sends `ManifestRequest` and
   receives `ManifestResponse` or `Error`.

### Updating the offer

Either peer may send `Hello` again on the main stream at any time, when the folders it offers
this peer change (a folder was added or removed, or the peer's membership changed). The
certificate must be for the same device and user as in the first `Hello`. After the first
exchange, both sides offer only folders the peer is a member of: the connecting peer, whose
first `Hello` named every folder it syncs, sends an updated `Hello` as soon as its offer for this
peer differs from what it sent, and either side does the same if its folders changed while the
handshake was in flight.

On sending or receiving an updated `Hello`, each side recomputes the shared set as the
intersection of the two latest offers. It sends `IndexRequest` for folders that became shared,
and stops sending updates for, and ignores `IndexBatch` messages about, folders that are no
longer shared.

After the initial exchange the stream stays open, and new local versions are pushed as
`IndexBatch` messages.

## Conflict resolution

When two versions of an entry are concurrent, every replica picks the same winner:

1. A live version (anything but `Tombstone`) beats a `Tombstone`: edits win over deletes.
2. Otherwise the higher `modified_ms` wins.
3. Otherwise the greater `author`, compared as bytes, wins.
4. Otherwise (the same author produced two concurrent versions, which is a bug) the greater
   postcard encoding of the `EntryVersion` wins.

The loser is kept as a conflict copy until a later version supersedes both. When a
`Tombstone` supersedes a live version, the live version moves to the recycle bin, which
keeps it for the retention period.

## Content addressing (informative)

The reference chunker uses FastCDC (2020) with minimum 64 KiB, average 256 KiB and maximum
1 MiB chunks. Manifests list chunks explicitly, so peers using other parameters still
interoperate; they only deduplicate less.

## Open questions

- Certificate revocation and changes to a folder's member list.
- Flow control for large indexes.
