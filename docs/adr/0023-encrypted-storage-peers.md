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

## Amendments before the specs (2026-10-18)

This ADR predates author vouching (ADR 0024, enforced from 0.21), device revocation and the
time of loss (ADR 0040), per-folder databases (ADR 0034) and confirmed copies (ADR 0022). These
amendments fit it to them; where they differ from the decision above, they win.

- **Encryption keys are signed by the device.** `Signed<EncryptionKey { device, key, issued_ms
  }>` is signed by `device`'s identity key, which its user certified, rather than by the user
  key: the user key stays on one device (ADR 0015), and devices paired before Phase 6 can
  publish a key without it. A device's newest encryption key by `issued_ms` replaces older ones.
- **Keys are identified, not only numbered.** Each folder key has a 16-byte `key id` besides its
  generation, derived from the key (encryption-v1), so a device granted a key can check it. Grants and sealed versions name both. Two devices that rotate at once
  make two keys of one generation; both are granted to the same devices, so both stay readable,
  and every member uses the one with the lower key id for new content. Members keep every key
  they are granted.
- **Rotation also on revocation.** A revoked device held every generation it was granted, so a
  member device that learns a revocation of a device holding a grant rotates, exactly as when a
  user is removed: the next generation, granted to the devices of member users that are neither
  revoked nor removed. New content then uses it. As before, nothing already written is
  re-encrypted. A device that already holds a key of a later generation, granted only to such
  devices, does not rotate again, so a removal leads to one rotation unless devices make it at
  the same moment.
- **Every key is per generation.** The chunk-ID, chunk, nonce and metadata keys derive from one
  generation's folder key, not from the first. Content written after a rotation gets new chunk
  IDs, so deduplication works within a generation only, and a file changed after a rotation is
  uploaded whole once. In exchange, a device that was removed or revoked cannot test whether
  content added after it left matches content it knows.
- **Storage peers vouch too.** A storage peer verifies every version's signature and applies a
  version only if its author is vouched for by its operator's member list for the folder and the
  revocations it knows, as members do (ADR 0024). It keeps and serves the certificates of the
  authors it stores, keeps every revocation it learns and passes them on in `Hello`, so
  revocations reach members that never meet. It never takes over or withdraws anything: it
  cannot read what it would take over.
- **A storage peer's "cannot vouch" counts for nothing.** Its member list is its operator's and
  may disagree with the members'. A member never takes over versions because a storage peer
  answered that it cannot vouch for their author; only a member's answer counts. A peer is a
  storage peer for a folder when its user is not a member of the folder.
- **Withdrawing what is not held.** ADR 0040 withdraws a revoked device's versions whose content
  a replica does not hold, after an exchange, trusting that a peer holding the content has taken
  it over by then. A storage peer holds content without taking it over, so in an encrypted
  folder a member first fetches the content of such a version, from any peer, storage peers
  included: if it gets it, it takes the version over; only if every peer it reaches lacks the
  content does it withdraw the version.
- **No arrival time from a storage peer.** ADR 0040 places a version before or after a time of
  loss by when each replica received it. A version that waited on a storage peer arrives late
  without being late, so a member records no arrival time for a version it first received from
  a storage peer: such versions are taken over where held and never withdrawn. Members that meet
  only through storage peers withdraw little on a time of loss, but keep the work made before
  it; revocation still stops everything the device makes afterwards.
- **Conflict resolution needs the decrypted version.** Rule 4 of sync/1 (the greater encoding
  wins) compares decrypted `EntryVersion`s, which every member computes alike; two sealed versions
  that decrypt alike are ordered by their conflict copy IDs (see "On member devices"). The other
  rules also use the decrypted fields, so only members resolve conflicts, as decided above.
- **Members check what they decrypt.** A member checks that a decrypted `EntryVersion` agrees
  with its sealed version (folder, entry, version vector, author) and satisfies sync/1's path
  rules, and drops it otherwise: a storage peer cannot check these, and the author's device
  could be compromised.
- **Confirmed copies on storage peers.** A storage peer that is a full replica of a folder
  counts as a confirmed copy (ADR 0022) of content only while another member device holds a
  grant for the key that content is sealed with. Otherwise removing originals after backup could
  leave the only key on the phone that removed them. Grants sync, so each device can tell.
- **Sizes.** sync/1 keeps chunk lists out of the index. A sealed version needs its chunk IDs in
  the clear, so that a storage peer knows what to keep, and the specs bound them: a version
  lists at most 4096 chunk IDs itself (1 GiB at the average chunk size, 4 GiB at the largest);
  a larger file's list is stored in list blobs, plaintext lists of chunk IDs, which a storage
  peer can read and keep, and which name nothing but keyed IDs. Index batches are limited by
  size as well as by count.
- **Members keep the sealed version.** A member stores the signed sealed version as received,
  to serve it on, beside the decrypted version it applies: a change to the folder database
  (ADR 0034). Storage peers store only sealed versions.
- **The simulation runs encrypted folders.** The simulation (ADR 0036) gains encrypted folders
  and a storage-peer device in the same steps as the code that implements them.

## On member devices (2026-10-18)

How a member device stores an encrypted folder, decided before the engine is changed, since it
fixes the folder database (ADR 0034):

- **Plaintext inside, sealed at the edge.** A member keeps chunks as today, by the BLAKE3 hash of
  their plaintext, and keeps per folder which chunk ID, under which key, each one is. It seals a
  chunk when it serves it, which gives the same bytes every time, and opens and checks one when
  it fetches it, then stores it by its plaintext hash. Manifests, assembly, materialising,
  eviction and confirmed copies work on plaintext as before.
- **One folder model.** The folder's versions (`meta`) are the decrypted `EntryVersion`s, as for a
  plaintext folder. What each was received as is either a signed `EntryVersion` or a signed
  `SealedVersion`; that form is what the database keeps beside the decrypted version, what is
  served, and what a conflict copy's ID hashes.
- **Rule 4 in practice.** Two concurrent versions that tie on rules 1 to 3 are ordered by the
  encoding of their decrypted `EntryVersion`, which every member computes alike, and only if
  those are equal too, by their conflict copy IDs, which hash their signed `SealedVersion` bodies
  and are kept in memory anyway (ADR 0029), so no sealed bytes are (ADR 0035).
- **Items besides versions.** Encryption keys, grants, and sealed versions a member cannot open
  yet are kept in the folder database too, under sequences from the same counter as versions,
  so one index serves them all. A sealed version not yet openable is dropped once a version it
  holds supersedes it, judged by vector without opening it; once opened it is applied like any
  version and leaves this set.
- **Keys.** A member keeps the folder keys it holds in the folder's database, which already sits
  beside the plaintext. The device's X25519 secret is kept with its identity key, so a folder
  rebuilt from peers (ADR 0034) gets its keys back from the grants the member made to itself.
- **Order of work.** Storage and the index first; then local changes sealed and remote versions
  opened; then keys, grants and rotation; then blobs/2; then creating and joining from the CLI
  and API. Each extends the simulation. Step 5 is done when two members sync an encrypted folder
  directly, with no storage peer.

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
- A storage peer's operator must remove a user from its member list too: until then it still
  serves that user's devices the sealed content they were already able to read.
- Rotations cost one upload of each file changed afterwards, and two concurrent rotations leave
  two keys of one generation that members keep for good.
