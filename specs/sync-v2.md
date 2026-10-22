# Sync protocol, version 2: encrypted folders

ALPN: `tidefell/sync/2`

Status: draft. Messages are to be implemented in `crates/proto`; golden encodings are in
[`vectors/sync-v2.json`](vectors/sync-v2.json), made by
[`vectors/gen/sync_v2.py`](vectors/gen/sync_v2.py) from this text. See ADR 0023, as amended.

## Purpose

Exchange the metadata of **encrypted folders** between their member devices and the storage
peers that hold them. sync/2 does not replace sync/1: a plaintext folder uses sync/1 and an
encrypted folder sync/2, with every peer, so each kind of folder has one wire format. A device
that syncs both kinds offers both ALPNs. ADR 0026's deprecation of earlier versions does not
apply between them.

Everything sync/1 says holds for sync/2 except where this page says otherwise: its encoding,
identifiers, version vectors, index sequences, signed envelopes, certificates and revocations,
the exchange and the updating of offers. Keys and sealing are in
[encryption-v1.md](encryption-v1.md); chunks and list blobs travel over `tidefell/blobs/2`
([blobs-v2.md](blobs-v2.md)).

## Transport

As sync/1, with a main stream and certificate streams. There are no manifest streams: a
version's chunk IDs travel in the version, and their lengths in its sealed payload.

## Devices

A folder's **member users** are as in sync/1. A **member device** is a device certified by a
member user. It holds the folder's keys, decrypts the folder into its directory, and can read and
change it.

A **storage peer** of a folder is a device whose user is not a member of the folder. Its operator
lists, for each folder it stores, the member users whose devices may sync it. Member devices list
the device IDs of the folder's storage peers. A storage peer stores and serves what members
write, without being able to read it, and never changes anything.

Each side of a session lists in `Hello` only folders the peer may sync with it:

- a member device, the folders whose member users include the peer's user, and those that list
  the peer's device as a storage peer;
- a storage peer, the folders whose member list, as its operator configured it, includes the
  peer's user.

Two storage peers do not sync with each other.

## Vouching

A replica applies a version only if its author is vouched for, as in sync/1: the replica's own
device, or a device certified by a user who is a member of the folder now, that user not having
revoked it. A storage peer judges by its operator's member list for the folder and the
revocations it knows. It keeps the certificates of the authors whose versions it stores, serves
them on certificate streams, and keeps and passes on every revocation it learns, so revocations
reach members that never meet.

A storage peer's member list is its operator's and may disagree with the members'. A member
therefore treats a storage peer's `UnknownDevice` or `NotAMember` answer to a certificate request
as no answer: it holds the batch's folder for the session, as for an unanswered request, and never
takes over a version because a storage peer cannot vouch for its author. A peer is a storage peer
for this purpose when its certificate's user is not a member of the folder.

Members take over the versions of authors no longer vouched for as in sync/1. Storage peers take
over nothing: they cannot read what they would take over. Before a member withdraws a revoked
device's version whose content it does not hold (ADR 0040), it tries to fetch that content from
its peers, storage peers included; if it gets every chunk, it takes the version over instead. It
withdraws the version only if at least one peer answered whether it holds the content, and every
peer that answered lacked some of it.

A time of loss (ADR 0040) is judged by when each replica received a version. A version a member
first receives from a storage peer may have been made long before it arrived, so the member
records no arrival time for it: it cannot be placed before or after a time of loss, and is taken
over where held and never withdrawn. In a folder whose members meet only through storage peers, a
time of loss therefore withdraws little; revoking the device still stops everything it makes
afterwards.

## Items

A folder's index holds three kinds of item, each with its own index sequence number:

| Index | Item | Signed kind | Signer must be |
| --- | --- | --- | --- |
| 0 | `Version` | `sync/2/sealed-version` | `SealedVersion.author` |
| 1 | `EncryptionKey` | `sync/2/encryption-key` | `EncryptionKey.device` |
| 2 | `Grant` | `sync/2/key-grant` | a member device, vouched for |

`EncryptionKey` and `KeyGrant` are defined in [encryption-v1.md](encryption-v1.md).

### Sealed versions

`SealedVersion`:

| Field | Type | Notes |
| --- | --- | --- |
| `folder` | `FolderId` | |
| `entry` | `EntryId` | |
| `author` | `DeviceId` | Must equal the envelope signer |
| `generation` | u64 | Of the folder key it is sealed with; at least 1 |
| `key` | `KeyId` (16 bytes) | The key it is sealed with |
| `chunks` | `ChunkList` | See below |
| `sealed` | bytes | The sealed payload (encryption-v1.md): at least 104 bytes |
| `version` | `VersionVector` | Last field, encoded as in sync/1's `EntryVersion` |

`ChunkList` variants: `0 Inline(Vec<ChunkId>)`, at most 4096 chunk IDs, and `1 Lists(Vec<ListId>)`,
2 to 256 list IDs, used only for files of more than 4096 chunks. `ChunkId` and `ListId` are 32
bytes. A version that is not a file has `Inline` with no chunk IDs. A signed sealed version is at
most 1 MiB encoded.

Anyone can check a sealed version's signature, its signer, its folder, the bounds above and its
version vector, and must drop a version that fails. A member that holds the key also opens its
payload and checks that:

- the payload decodes, using every byte;
- the path satisfies sync/1's path rules;
- for a `File`, the version has one chunk ID per entry of `chunk_lens`, each length is 1 to
  1 MiB, and the lengths add up to `size`; for any other kind, there are no chunk IDs and no
  lengths.

A version that fails is dropped as one that fails sync/1's checks. The decrypted version applied
is the `EntryVersion` with the sealed version's `folder`, `entry`, `author` and `version`, and the
payload's `path`, `kind` and `modified_ms`. A file's manifest is its chunk IDs with the payload's
lengths, and its content is checked against `content` when the file is assembled, as in sync/1.

A member that does not hold the key a version names keeps the sealed version, serves it on, and
applies it once it is granted the key. Members keep each sealed version as received, beside what
they decrypt from it, so that they serve exactly the signed bytes.

### Conflicts

Members resolve conflicts as sync/1 does, with one change to rule 4: of two versions that tie on
rules 1 to 3, the greater encoding of the decrypted `EntryVersion` wins, as in sync/1, and if
those are equal, the greater conflict copy ID: the first 16 bytes of
BLAKE3(`"tidefell conflict copy\n"` || the signed `SealedVersion` body), compared as bytes, which
differs between any two sealed versions (ADR 0029 names a kept conflict copy by it). A version a
member cannot open yet takes no part in choosing the winner; once it is granted the key, it
resolves the entry again. A storage peer does not resolve conflicts. It keeps every version no
other version it holds supersedes.

### Keys and grants

- **Encryption keys.** A device that syncs an encrypted folder publishes its encryption key in
  it. An `EncryptionKey` that a later one of the same device replaces is no longer in the index.
- **Granting.** A member device that holds a folder key grants it to every member device that is
  vouched for, has published an encryption key, and holds no grant of that key to its current
  encryption key. Storage peers are never granted keys: their users are not members.
- **Keeping grants.** Every replica keeps and serves every grant whose signer is a member device
  it vouches for, whoever the recipient, so a grant reaches its recipient through any peer. It
  drops grants from other signers. A device opens only grants to itself and its current
  encryption key, and ignores one that does not open or whose key does not match its key ID.
- **Rotation.** A member device that holds the newest key makes the next generation when its
  member list drops a user, or when it learns a revocation of a device holding a grant of that
  key, and grants it to the member devices that remain. It does not when it already holds a key
  of a later generation that was granted only to devices that remain.
- **Using keys.** A member seals new content with the newest generation it holds, and of that
  generation the key with the lower key ID. It keeps every key it holds.
- Grants and encryption keys, once in an index, stay there, except encryption keys that a later
  one replaces.

## Messages

| Index | Message | Fields |
| --- | --- | --- |
| 0 | `Hello` | `certificate: Signed<DeviceCertificate>`, `folders: Vec<FolderId>`, `clock_ms: i64`, `revocations: Vec<Signed<DeviceRevocation>>` |
| 1 | `IndexRequest` | `folder: FolderId`, `epoch: u64`, `since: u64` |
| 2 | `IndexBatch` | `folder: FolderId`, `epoch: u64`, `up_to: u64`, `items: Vec<Item>`, `last: bool` |
| 3 | `CertificateRequest` | `folder: FolderId`, `device: DeviceId` |
| 4 | `CertificateResponse` | `certificate: Signed<DeviceCertificate>` |
| 5 | `Error` | `code: ErrorCode` |

`Item` variants: `0 Version(Signed<SealedVersion>)`, `1 EncryptionKey(Signed<EncryptionKey>)`,
`2 Grant(Signed<KeyGrant>)`.

`ErrorCode` variants: `0 NotAMember`, `1 UnknownFolder`, `2 ProtocolViolation`,
`3 UnknownDevice`.

Every field is present in every message. `Hello`'s `revocations` hold at most 256.

## Exchange

As sync/1, with items in place of versions:

- The responder sends every item in its index whose sequence is greater than `since`: versions
  that no version it holds supersedes, the newest encryption key of each device, and every
  grant. An `IndexBatch` holds at most 512 items and is at most 4 MiB encoded.
- The requester checks each item and applies it as above, holding versions whose author is not
  vouched for yet as sync/1 does. It records `epoch` and `up_to` only after handling the batch
  marked `last`.

## What a storage peer sees (informative)

Folder IDs, member user and device IDs and certificates, entry IDs, version vectors, which device
wrote each version and when it arrived, chunk counts, list blobs, sealed chunk lengths (each to
within 4 KiB) and sealed payload lengths (to within 64 bytes), and who holds a grant of which
key. Chunk IDs are the same for the same content within a generation, so it also sees which
versions and entries share chunks: copies, and edits that keep most of a file. Not paths, names,
contents, exact sizes or keys. Members' priorities ([storage-v1.md](storage-v1.md)) show it,
besides, which entries they rank above others.

## Open questions

- Telling a storage peer that a sealed chunk it serves is corrupt, so that it fetches it again.
