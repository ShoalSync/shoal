# Folder encryption, version 1

Status: draft. The derivations and sealing are to be implemented in `crates/crypto`; golden
values are in [`vectors/encryption-v1.json`](vectors/encryption-v1.json), made by
[`vectors/gen/encryption_v1.py`](vectors/gen/encryption_v1.py), a second implementation written
from this text. See ADR 0023, as amended.

## Purpose

The keys and formats of encrypted folders: what members of the folder can read and storage
peers cannot. The messages that carry them are in [sync-v2.md](sync-v2.md) and
[blobs-v2.md](blobs-v2.md).

## Primitives

| Name | Definition |
| --- | --- |
| `derive_key(context, key)` | BLAKE3 in key derivation mode, 32 bytes out |
| `keyed_hash(key, data)` | BLAKE3 in keyed mode, 32 bytes out |
| `hash(data)` | BLAKE3, 32 bytes out |
| `seal(key, nonce, aad, plaintext)` | XChaCha20-Poly1305: the ciphertext, then the 16-byte tag |
| HPKE | RFC 9180 base mode with DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305 |

Integers inside the derivation inputs below are big-endian; everything carried in a message is
encoded as in sync/1 (postcard).

## Folder keys

A folder key is 32 random bytes. Each has a **generation**, a `u64` starting at 1, and a
16-byte **key ID** derived from the key:

```
key_id           = derive_key("shoal 2026-10-24 folder key id", folder_key)[0..16]
chunk_id_key     = derive_key("shoal 2026-10-24 chunk id", folder_key)
chunk_key        = derive_key("shoal 2026-10-24 chunk key", folder_key)
chunk_nonce_key  = derive_key("shoal 2026-10-24 chunk nonce", folder_key)
metadata_key     = derive_key("shoal 2026-10-24 metadata key", folder_key)
```

The key ID names a key without revealing it, and lets a device that receives a key check that it
is the key it was told about. Two keys of one generation, made by devices rotating at the same
moment, have different key IDs. A device keeps every folder key it is granted. New content uses
the key of the highest generation it holds, and of that generation the key with the lower key ID,
compared as bytes.

## Chunks

A chunk's plaintext is at most 1 MiB. Its **chunk ID** is keyed by the folder key, so that only
members can compute it:

```
chunk_id = keyed_hash(chunk_id_key, plaintext)
nonce    = keyed_hash(chunk_nonce_key, chunk_id)[0..24]
padded   = plaintext || 0x01 || 0x00 * n      (the least n ≥ 0 making len(padded) a multiple of 4096)
aad      = "shoal/chunk/1" || folder || key_id || chunk_id
sealed   = seal(chunk_key, nonce, aad, padded)
```

`folder` is the 32-byte `FolderId`. The nonce depends only on the key and the plaintext, so the
same plaintext always seals to the same bytes and deduplicates within a generation; different
plaintexts get different nonces. The padding hides a chunk's exact length to within 4 KiB. The
sealed chunk carries no nonce: a reader derives it from the chunk ID it asked for.

To open a sealed chunk under a key, a member derives the nonce from the chunk ID, opens it with
the same `aad`, and removes the padding: the trailing 0x00 bytes, at most 4095 of them, and the
0x01 before them, which must be there. It checks that the plaintext is at most 1 MiB and that
`keyed_hash(chunk_id_key, plaintext)` equals the chunk ID. A chunk that fails any check is treated as missing from the peer that served it. A storage
peer holds and serves sealed chunks without opening them.

## Chunk lists

A file's chunk IDs are in its sealed version (sync/2) in the clear, so that a storage peer knows
what to keep. A version lists up to 4096 chunk IDs itself. A file with more has them in **list
blobs**: a list blob is the postcard encoding of a `Vec<ChunkId>` of at most 4096 chunk IDs, and
its **list ID** is `hash(list blob)`. Every list of a file but the last holds exactly 4096 chunk
IDs. A list blob reveals nothing a sealed version does not, so it is stored and served in the
clear, and anyone can check it against its ID.

## Sealed metadata

What a member writes to disk, and a storage peer must not see, is sealed in each version:

```
SealedPayload = path: string || kind: EntryKind || modified_ms: i64 || chunk_lens: Vec<u32>
```

`path`, `kind` and `modified_ms` are as in sync/1's `EntryVersion`. For a `File`, `chunk_lens`
lists the plaintext length of each chunk, in the order of the version's chunk IDs: their sum is
the file's `size`, and the BLAKE3 hash of the concatenated plaintexts is its `content`. For any
other kind it is empty.

```
padded = postcard(SealedPayload) || 0x01 || 0x00 * n   (the least n ≥ 0 making len(padded) a multiple of 64)
aad    = "shoal/version/1" || folder || entry || author || key_id
sealed = nonce || seal(metadata_key, nonce, aad, padded)
```

`nonce` is 24 random bytes, chosen anew for each version. `entry` is the 16-byte `EntryId` and
`author` the 32-byte `DeviceId` of the version that carries it. The padding hides the length of
the path to within 64 bytes.

A member opens the payload with the key the version names, removes the padding as for chunks
(at most 63 trailing 0x00 bytes), decodes the payload, which must use every byte, and checks it as
sync/2 says before applying the version.

## Encryption keys

Each device has an X25519 key pair for receiving folder keys, separate from its identity key. It
publishes the public key, signed by its own device key:

```
EncryptionKey = device: DeviceId || key: [u8; 32] || issued_ms: i64
```

Kind `sync/2/encryption-key`; the signer must equal `device`. A device's encryption key with the
greater `issued_ms` replaces any earlier one; of two with the same time, the greater encoding.
A device keeps its private key with its identity key, not in a folder's state, so it survives a
folder being rebuilt (ADR 0034).

## Key grants

A grant gives one folder key to one device:

```
KeyGrant = folder: FolderId || generation: u64 || key: KeyId || recipient: DeviceId
           || recipient_key: [u8; 32] || enc: [u8; 32] || sealed: bytes
```

Kind `sync/2/key-grant`; signed by the granting device. `recipient_key` is the encryption key
the folder key is sealed to, and `enc` and `sealed` are HPKE's encapsulated key and ciphertext:

```
info          = "shoal/key-grant/1" || folder || u64(generation) || key || recipient || recipient_key
(enc, sealed) = HPKE.Seal(recipient_key, info, aad = "", plaintext = folder_key)
```

`sealed` is 48 bytes: the 32-byte key and the tag. The recipient opens it with its private key
for `recipient_key`, and checks that the key ID derived from the folder key equals `key`. A
grant that fails is ignored. A device that creates a folder key grants it to itself as well as
to the others, so that it can recover the key from a peer if its own state is lost.

Who may grant, and when, is in sync/2.

## Vectors

[`vectors/encryption-v1.json`](vectors/encryption-v1.json) holds, from fixed inputs:

- a folder key's ID and subkeys;
- two chunks' IDs, nonces, padded lengths and sealed bytes;
- a list blob and its ID;
- a sealed payload, with a fixed nonce;
- an HPKE grant, with a fixed ephemeral key (HPKE's `DeriveKeyPair` from fixed input keying
  material), and the signed `EncryptionKey` and `KeyGrant`.

- under `refused`, inputs a reader must refuse: a chunk whose padding lacks its terminator, a
  chunk that opens but does not match its ID, a payload with bytes left over, a grant whose key
  does not match its key ID, and a list blob that does not match its ID.

The generator also checks its HPKE against RFC 9180's own test vector for this suite (appendix
A.2.1).
