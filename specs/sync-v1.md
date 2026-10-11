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

The author of every version a replica applies must be vouched for: it must have a device
certificate signed by a user who is a member of the folder (ADR 0024). Replicas keep the
certificates of the authors of the versions they store and serve them on certificate streams
(see Exchange). Versions later received from the devices of a user who has been removed from
the folder are refused; versions already applied stay.

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

A `VersionVector` is the set of changes to an entry that a version has seen. Each device numbers
its changes to an entry 1, 2, 3 and so on. A vector holds, per device, the highest change seen,
and the ranges of lower changes not seen, called holes. It is encoded as a struct of:

1. a postcard map `DeviceId → u64` of the highest change seen, sorted by key, with no zero
   counters;
2. a list of holes, each `(DeviceId, first, last)`: an inclusive range of that device's changes
   not seen. Holes are sorted by device and then `first`, do not overlap, are not adjacent, and
   lie between 1 and one below that device's highest change. There are at most 64.

So every vector has exactly one encoding. No counter may exceed 2^62; a version whose vector
does, or whose holes break these rules, is invalid. In an `EntryVersion`, the vector is the last
field; when it has no holes, the empty list (a single `0x00` byte) is left out, so such a version
ends with the map. A body that writes out an empty list, or has bytes after the version, is
invalid.

Version `a` supersedes `b` if `a` has seen every change `b` has, and at least one more. If
neither supersedes the other, they are concurrent (a conflict). Two versions with equal vectors
but different encodings are also concurrent; identical encodings are the same version.

Version vectors are per entry and are used only to order versions of that entry. For every
file it has written to or scanned from disk, a replica records the vector that content
corresponds to: its base. A file written from the current version has as its base the current
version's vector, merged with the vectors of conflict copies that have the same content. A
device making a change sets the new vector to the base plus its next change, numbered one above
the highest change it has made to the entry. A change of its own that the base has not seen
stays unseen, as a hole. A device refuses a change that would take its counter past 2^62 or
leave more than 64 holes. A remote version that arrived but was not written to disk, because
the local file had changed, or because it lost to the current version, therefore stays
concurrent with the new version.

A device's counters are never reused, so a device's data must not be restored from a backup or
copied to another device; such a device re-pairs instead.

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
| 0 | `Hello` | `certificate: Signed<DeviceCertificate>`, `folders: Vec<FolderId>`, `clock_ms: Option<i64>` |
| 1 | `IndexRequest` | `folder: FolderId`, `epoch: u64`, `since: u64` |
| 2 | `IndexBatch` | `folder: FolderId`, `epoch: u64`, `up_to: u64`, `versions: Vec<Signed<EntryVersion>>`, `last: bool` |
| 3 | `ManifestRequest` | `folder: FolderId`, `content: ContentHash` |
| 4 | `ManifestResponse` | `manifest: Manifest` |
| 5 | `Error` | `code: ErrorCode` |
| 6 | `CertificateRequest` | `folder: FolderId`, `device: DeviceId` |
| 7 | `CertificateResponse` | `certificate: Signed<DeviceCertificate>` |

`Hello`'s `clock_ms` is the sender's clock when it sent the message, in milliseconds since the
Unix epoch, so each side can tell how far apart their clocks are (ADR 0030). It was appended
later: a `Hello` whose frame ends after `folders` is read with `clock_ms` absent. Every other
message must be complete.

`EntryVersion`:

| Field | Type | Notes |
| --- | --- | --- |
| `folder` | `FolderId` | |
| `entry` | `EntryId` | |
| `path` | string | Relative, `/`-separated, NFC; non-empty components; no `.` or `..` components; first component not `.tidefell`; no `\` or NUL |
| `kind` | `EntryKind` | See below |
| `modified_ms` | i64 | Milliseconds since Unix epoch, UTC |
| `author` | `DeviceId` | Must equal the envelope signer |
| `version` | `VersionVector` | Vector after this change |

`EntryKind` variants: `0 File { content: ContentHash, size: u64, executable: bool }`,
`1 Directory`, `2 Symlink { target: string }`, `3 Tombstone`.

`Manifest`: `content: ContentHash`, `chunks: Vec<ChunkRef { hash: ChunkHash, len: u32 }>`.
`content` must equal BLAKE3 of the concatenated chunks, and the sum of the chunk lengths must
equal the `size` of the version whose content it describes.

`ErrorCode` variants: `0 NotAMember`, `1 UnknownFolder`, `2 UnknownManifest`,
`3 ProtocolViolation`, `4 UnknownDevice`.

Messages 6 and 7 and error code 4 were appended by ADR 0024. A peer that does not know them
cannot decode a certificate request and closes that stream.

## Exchange

Before the handshake completes, a peer reads at most 64 KiB for a message, and the handshake
must complete within 10 seconds.

1. The connecting peer sends `Hello` listing the folders it syncs. The accepting peer checks
   the certificate (see Membership) and replies with `Hello` listing only the folders the
   connecting peer is a member of. Each side then proceeds only with folders that both sides
   listed and for which the peer is a member.
2. For each shared folder, each side sends `IndexRequest` with the epoch and sequence it last
   received from this peer (both 0 the first time).
3. The responder sends, in sequence order, every version currently in its index (current
   versions and conflict copies) whose sequence is greater than `since`, or all of them if
   `epoch` does not match its own. It uses one or more `IndexBatch` messages of at most 512
   versions each, the last with `last = true`. Superseded versions are not sent. A peer
   writes batches from a task separate from the one reading the stream, so that reading never
   waits on writing.
4. The requester verifies each version (signature, signer equals author, folder, path rules,
   counter ceiling) and applies it if its author is vouched for (see Membership); otherwise
   it holds the version and requests the author's certificate. A version that fails these
   checks is dropped; the rest of the batch is applied. It records `epoch` and `up_to`
   only after handling the batch marked `last`.
5. A peer that needs a file's chunk list opens a manifest stream, sends `ManifestRequest` and
   receives `ManifestResponse` or `Error`. A manifest that fails the checks above, or fails
   when the file is assembled, is not kept.
6. A peer that needs an author's certificate opens a certificate stream, sends
   `CertificateRequest` and receives `CertificateResponse`, or `Error` with `UnknownDevice`.

A requester that gets no response to a request within 30 seconds treats the request as failed.

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

After the initial exchange the stream stays open, and new versions are pushed as `IndexBatch`
messages. A peer does not push a version to the peer it received that version from.

## Conflict resolution

When two versions of an entry are concurrent, every replica picks the same winner:

1. A live version (anything but `Tombstone`) beats a `Tombstone`: edits win over deletes.
2. Otherwise the higher `modified_ms` wins.
3. Otherwise the greater `author`, compared as bytes, wins.
4. Otherwise (the same author produced two concurrent versions, as a device does when it edits
   a file whose conflict with its own version is not resolved) the greater encoding of the
   `EntryVersion`, as signed, wins.

Rule 4 also decides between two versions with equal vectors and different encodings.

The loser is kept as a conflict copy until a later version supersedes both. Since a change
made on disk builds only on the content it changed, that happens when the conflict is resolved
(ADR 0029), or when a version made from the conflict copy's own content supersedes it. A
conflict copy with the same content as the winner is not a conflict: a change to the winner
supersedes it too. When a
`Tombstone` supersedes a live version, the live version moves to the recycle bin, which
keeps it for the retention period.

### Entries at the same path

Different entries can have the same path, for example when two devices create the same file
while apart. Every replica writes them to disk the same way, without making new versions:

1. Among live entries whose paths are equal, the one with the lower `EntryId`, compared as
   bytes, keeps the path.
2. A file or symlink whose path is also a directory, whether a `Directory` entry or implied by
   entries below it, gives way to the directory. Two directories at one path are one directory.
3. An entry that gives way is written as `<stem> (conflict <id>)<extension>` in the same
   directory, where `<id>` is the first 8 hex digits of its `EntryId`, or all 32 if that name is
   also taken. `<extension>` is the part of the name from its last `.`, if that is not the
   first character.

The first local change to an entry that gave way, including a rename, records its current name
as its path.

A replica whose filesystem cannot store two names apart, such as one that ignores case, applies
these rules to paths as it compares them, so that both entries are written; directories whose
paths compare equal are one directory, spelled as the entry with the lowest `EntryId` beneath it
spells it. This changes only where that replica writes them. A local change to an entry that
gave way only there keeps the entry's path, and a replica does not write an entry whose name its
filesystem refuses. (Informative: the reference implementation folds case with Unicode's
lowercase mapping.)

## Content addressing (informative)

The reference chunker uses FastCDC (2020) with minimum 64 KiB, average 256 KiB and maximum
1 MiB chunks. Manifests list chunks explicitly, so peers using other parameters still
interoperate; they only deduplicate less.

## Open questions

- Certificate revocation.
