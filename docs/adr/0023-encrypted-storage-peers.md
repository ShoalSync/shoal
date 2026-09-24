# 0023: Encrypted folders and storage peers

- Status: accepted
- Date: 2026-09-24

## Context

ADR 0018 chose a self-run always-on peer for Phase 5 and left option A for later: a storage
peer that holds and serves a folder without being able to read it. Anyone could run one,
including a friend or a provider, without being trusted with the files. This ADR fixes the
design so that the specs can be written first. It builds on ADR 0018's sketch.

Constraints from the existing design:

- Every device of a member user holds plaintext. Only storage peers must not.
- Membership is configured per device, and certificate revocation is not specified (sync/1
  open questions). User keys never leave their device (ADR 0015).
- Chunks are content-addressed by BLAKE3 of their plaintext. Anyone who holds the chunks can
  therefore test whether a folder contains a known file.

## Decision

### Folders

- A folder is **plaintext** or **encrypted**, chosen when it is created and fixed after that.
  Converting means creating a new folder. Plaintext folders are unchanged and keep sync/1
  and blobs/1.
- An encrypted folder uses `tidefell/sync/2` and `tidefell/blobs/2` with every peer, members
  and storage peers alike, so there is one wire format per folder kind.
- Member devices keep decrypted files on disk as today, so the CLI, FUSE mounts, Android, and
  the web peer on a member device all work unchanged.

### Keys

- Each encrypted folder has a **folder key**: 32 random bytes with a generation number,
  starting at 1. Subkeys are derived from it with BLAKE3 `derive_key`, one context string per
  use: chunk IDs, chunk encryption, chunk nonces and metadata.
- Each device has an **X25519 encryption key**, separate from its Ed25519 identity key. It is
  published as `Signed<EncryptionKey { device, key, issued_ms }>`, signed by the device's
  user like a device certificate.
- A **key grant** gives one generation of a folder key to one device:
  `Signed<KeyGrant { folder, generation, recipient, sealed }>`. The folder key is sealed to
  the recipient's encryption key with HPKE (RFC 9180, base mode,
  DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305). The `info` binds the folder and
  generation. Grants are signed by the granting member device and sync like entry versions.
  Storage peers store and forward them without being able to open them.
- **Joining.** A new member device publishes its encryption key in the folder. Any member
  device that holds the key, sees the new device's certificate from a member user, and has no
  grant for it yet issues one. This also works asynchronously through a storage peer: the
  phone publishes its key there, the laptop grants it later, and the phone picks the grant up
  later again.
- **Removing a member.** A member device whose member list drops a user creates the next
  generation, and grants it only to the devices of the remaining users. New content uses the
  newest generation a device holds. Old content is not re-encrypted, so a removed member keeps
  whatever it already had. Proper revocation waits for the sync/1 open question to be settled.

### Content

- **Chunk IDs** are `BLAKE3-keyed(chunk-id key, plaintext)`. Deduplication still works within
  a folder, but a storage peer cannot test whether a folder contains a known file.
- **Chunks** are sealed with XChaCha20-Poly1305 under the chunk key. The nonce is derived from
  the plaintext with a keyed hash, so the same chunk always gives the same ciphertext and
  deduplicates. The AEAD tag authenticates chunks served by an untrusted storage peer.
- **Entry versions** travel as `Signed<SealedVersion>`, signed by the author device:
  - `folder`, `entry` and `version`, the version vector;
  - the key `generation`;
  - `chunks`, the list of chunk IDs;
  - `sealed`, the encrypted `EntryVersion` and manifest (random nonce, metadata key).
- A storage peer can see:
  - entry IDs, version vectors and which device wrote each version;
  - chunk counts and approximate sizes;
  - member user IDs and device IDs;
  - when changes happen.
  
  It cannot see paths, names, contents or exact sizes. The spec states this.

### Storage peers

- A storage peer is `tidefelld` with `[storage]` configured. It is certified by its operator's
  user, who is not a member of the folders it stores, so no member device ever grants it a key.
- Its operator configures each stored folder with:
  - the folder ID;
  - the member users whose devices may sync it;
  - a byte quota.
  
  Members add the storage peer's device ID to their folder settings so their devices connect
  to it.
- It keeps the newest versions by version vector, and concurrent versions until a later one
  supersedes them. Conflict resolution happens on member devices. It keeps superseded
  versions and their chunks for its own retention period (30 days by default). This gives
  version history even for deletes, which it cannot tell apart from edits.
- It runs no web peer and no FUSE mount, and it cannot join a folder as a member.

### Implementation

- A new `crypto` crate holds the folder key derivations, chunk and metadata sealing and HPKE
  grants, on RustCrypto crates, with golden vectors.
- `proto` gains the v2 messages, `net` gains sync/2 and blobs/2, the engine gains encrypted
  folders on member devices, and `tidefelld` gains the storage mode.
- The infra flake gains `role-storage-peer`.

## Consequences

- Specs come first: sync/2, blobs/2 and the key formats, each with vectors, are reviewed
  before any code.
- Deduplication across folders is lost for encrypted folders. Within a folder it still works.
- The web peer cannot serve a folder from a storage peer, only from a member device.
- Losing every member device loses the folder key, and with it access to the data on storage
  peers. The docs recommend keeping at least one full member replica, such as a NAS or a
  self-run always-on peer.
- Member changes are still per device, so devices can disagree about who is a member until
  membership sync is designed. Rotation limits the damage but does not remove it.
