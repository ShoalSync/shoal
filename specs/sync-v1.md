# Sync protocol, version 1

ALPN: `tidefell/sync/1`

Status: draft. Messages are implemented in `crates/proto` (`sync` module); golden encodings are
in [`vectors/sync-v1.json`](vectors/sync-v1.json).

## Purpose

Exchange folder metadata between two devices: which folders they share, which entry versions
each is missing, and the manifests (chunk lists) of file content. Chunk bytes are transferred
separately over `tidefell/blobs/1`.

## Transport

A QUIC connection negotiated with ALPN `tidefell/sync/1` over iroh. The connecting peer opens
one bidirectional stream. Both sides authenticate by their iroh endpoint key, which is the
`DeviceId`.

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
| `DeviceId` | 32 | Device Ed25519 public key |
| `FolderId` | 32 | Random, chosen when a folder is created |
| `EntryId` | 16 | Random, chosen when an entry is created; stable across renames |
| `ChunkHash` | 32 | BLAKE3 of the chunk bytes |
| `ContentHash` | 32 | BLAKE3 of the full file content |

## Version vectors

`VersionVector` is a map `DeviceId → u64` encoded as a postcard map sorted by key, with no zero
counters. Version `a` supersedes `b` if every counter in `a` is ≥ the one in `b`, and at least
one is greater. If neither supersedes the other, they are concurrent (a conflict).

## Signed envelopes

`EntryVersion` is always sent signed by its author:

```
Signed<T> = signer: DeviceId || body: bytes || signature: [u8; 64]
signed bytes = "tidefell/sig/1\0" || u8(len(kind)) || kind || body
```

`body` is the postcard encoding of `T`. `kind` for `EntryVersion` is `sync/1/entry-version`.
Signatures are Ed25519 and are verified in strict mode. Receivers reject a version whose
`signer` differs from `EntryVersion.author`.

## Messages

| Index | Message | Fields |
| --- | --- | --- |
| 0 | `Hello` | `device: DeviceId`, `folders: Vec<FolderId>` |
| 1 | `IndexRequest` | `folder: FolderId`, `since: VersionVector` |
| 2 | `IndexBatch` | `folder: FolderId`, `versions: Vec<Signed<EntryVersion>>`, `last: bool` |
| 3 | `ManifestRequest` | `folder: FolderId`, `content: ContentHash` |
| 4 | `ManifestResponse` | `manifest: Manifest` |
| 5 | `Error` | `code: ErrorCode` |

`EntryVersion`:

| Field | Type | Notes |
| --- | --- | --- |
| `folder` | `FolderId` | |
| `entry` | `EntryId` | |
| `path` | string | Relative, `/`-separated, NFC, no `.`/`..`, no leading `/` |
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

1. Both sides send `Hello`. Each side only proceeds with folders listed by both.
2. For each shared folder, each side sends `IndexRequest` with its summary vector (the merge
   of the vectors of every version it holds for that folder).
3. The responder sends every version it holds that the requester's summary does not
   dominate, as one or more `IndexBatch` messages, the last with `last = true`.
4. A peer that needs a file's chunk list sends `ManifestRequest` and receives
   `ManifestResponse` or `Error`.

After the initial exchange the stream stays open, and new local versions are pushed as
`IndexBatch` messages.

## Conflict resolution

When two versions of an entry are concurrent, every replica picks the same winner: the higher
`modified_ms`, then the greater `author` in byte order. The loser is kept as a conflict copy.
A `Tombstone` that wins moves the previous live version to the recycle bin.

## Content addressing (informative)

The reference chunker uses FastCDC (2020) with minimum 64 KiB, average 256 KiB and maximum
1 MiB chunks. Manifests list chunks explicitly, so peers using other parameters still
interoperate; they only deduplicate less.

## Open questions

- Membership proofs (device certificates signed by a user key) arrive with the `identity`
  crate in Phase 2.
- Flow control for large indexes.
