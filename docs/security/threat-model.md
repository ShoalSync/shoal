# Threat model

What Tidefell protects, from whom, and what it does not (ADR 0038). It describes the code as it
is: each gap is listed with what is planned for it, or why it stays. A change that touches any of
this updates this page in the same pull request; the pull request template asks.

## Assets

- **File contents.** The bytes of every synced file, and earlier versions kept in the recycle bin
  and history.
- **Paths and metadata.** File names and folders, sizes, modification times, which device made
  each version and when, and folder IDs, names and member lists.
- **Keys.** The user key, which certifies devices; each device key, which is the device's network
  identity and signs its versions; device certificates.
- **Web accounts.** Passwords of the web peer's local accounts (ADR 0011), their sessions, and
  share links (ADR 0019).
- **Availability.** That files keep syncing and are not lost: an attacker who cannot read a file
  may still try to delete it everywhere or stop it syncing.

## What protects them today

- **Encrypted, authenticated connections.** Devices talk over QUIC with TLS through iroh. A
  device's endpoint ID is its Ed25519 device key, so TLS proves which device is at the other end.
- **Device certificates.** A device certificate is the user key's signature over the device's ID
  and name. In the `sync/1` handshake each side checks the other's certificate and that it names
  the device at the other end of the connection. Certificates do not expire.
- **Folder membership.** Each replica keeps its own list of member users for each folder.
  Indexes, manifests, chunks, availability and replica questions are answered only for folders
  the asking device's user is a member of; others get "not a member". Membership is per user:
  every device certified by a member user is in.
- **Signed versions.** Every version is signed by the device that made it, and a replica checks on
  receipt, and again when it reads its database, that the signer is the version's author. A device
  cannot make versions in another device's name.
- **Keys at rest.** `device.key` and `user.key` are written owner-only (0600). The user key is
  needed only to certify new devices; a phone never holds it.
- **Local control.** `tidefelld`'s control API is a Unix socket readable and writable by its owner
  only (ADR 0010); there is no network API. Metrics, if turned on, are served on the address
  configured for them, without authentication: keep that address local.
- **Pairing** (ADR 0015). A pairing code carries a 128-bit secret and the new device's ID, lasts
  10 minutes, is used once, and stops working after 5 wrong proofs. The new device checks that the
  approver's certificate is valid and names the device it is talking to, and that the certificate
  it receives is for itself, from the approver's user. Both screens show a 6-digit confirmation
  for the person to compare.
- **The web peer** (ADR 0011, 0012, 0019). It is off unless configured, and expected behind TLS.
  Passwords are hashed with Argon2id at the argon2 crate's defaults (19 MiB, 2 passes); a login
  for an unknown name costs as much as one for a known name. Sessions last 12 hours in memory,
  with `HttpOnly`, `SameSite=Strict` and, by default, `Secure` cookies, and a CSRF token on every
  form after sign-in. Changing a password or role, or removing an account, ends its sessions.
  Pages forbid framing and other origins' scripts. Share links are off unless turned on; a link's
  128-bit token is stored only as a hash, and a link can have an expiry, a password, a download
  limit, and can be revoked. A link stops working when its creator loses access.
- **Diagnostics bundles** (ADR 0030) replace paths with keyed hashes, shorten IDs, reduce
  addresses to their kind and URLs to their host, unless the user chooses to include paths.
- **Android.** App data is excluded from cloud backups and device transfers.
- **Supply chain.** Dependencies are locked (`Cargo.lock`, `flake.lock`) and builds use them.
  `cargo-deny` refuses known advisories, yanked crates, unknown sources and licences outside the
  list. Workflow actions are pinned by commit and checked by zizmor. Renovate waits three days
  before proposing a release, and major updates need approval. Release assets have SHA-256 files.

## Adversaries

### A network attacker

Someone on the path between devices: a café's Wi-Fi, an ISP.

- **Cannot** read or change what devices send, or pose as a device: connections are TLS between
  keys, and a device without a certificate from a member user gets nothing.
- **Can** see that two addresses exchange encrypted traffic, when and how much, and block it.
- **Can see more on the local network.** With local discovery on (the default), a device
  announces itself by mDNS on the local network.

### A relay operator

The relays carry traffic between devices that cannot reach each other directly. By default these
are the iroh project's public relays; with address lookup on (the default) a device also
publishes its addresses through iroh's DNS service.

- **Cannot** read or change the traffic: it is end-to-end encrypted between the devices.
- **Can** see which devices (by public key) talk to each other, from which IP addresses, when and
  how much. The DNS service sees each device's addresses.
- **By design.** This is stated rather than hidden. Users who mind run their own relay (the infra
  repository's relay role, or `relay.enable` on an always-on peer) and turn address lookup off.
  A self-hosted relay serves any device that reaches it, unless it is given an access token
  (`accessTokenFile` in the infra relay module), which devices then present (`token_file`). The
  token is shared by every device, so it keeps strangers off the relay, not one device from
  another; a device that leaks it is handled by changing it everywhere. The Android app
  cannot present one yet.

### A member

Another user in a folder's member list, or any device they certify.

- **Can** read every file in the folder, and change or delete any of them on every replica.
  Deletes go to each device's recycle bin (ADR 0021), and conflicting edits are kept as conflict
  copies (ADR 0024), so mistakes and most malice can be undone within the retention period.
- **Can** certify as many devices as they like, and fill the folder with versions, limited only by
  batch sizes and the rate limits a replica sets.
- **Cannot** make versions in another device's name, or read folders they are not a member of.
- **Cannot** relay versions authored by devices of anyone (from 0.21): a version is applied only
  if its author has a certificate from a user who is a member of the folder now and has not
  revoked it (ADR 0024). Versions such authors made earlier are taken over by the replicas that
  hold them, so files stay while their authors stop counting. A member removed on one replica
  only is refused there and nowhere else; nothing is deleted on its account.
- **Cannot** make a file count as backed up by holding it on a device of theirs once removed:
  a confirmed copy (ADR 0022) counts only while the confirming device's user is a member.

### A removed member

A user taken out of a folder's member list.

- **Cannot** sync with a replica that removed them: it stops offering the folder, answering their
  index requests and serving them chunks.
- **Can** keep everything they already had (ADR 0024): nothing can take it back.
- **Gap: removal is per replica.** Member lists are each device's own configuration and do not
  sync, so a member must be removed on every replica. A replica that still lists them keeps
  syncing with them. No step is planned for this yet; the revocation ADR is the place to decide
  it.
- **Gap: their versions can still arrive.** Without author vouching (above), versions from a
  removed member's devices still apply when another member's device relays them.

### A lost or stolen device

- **Can** do everything its user's devices can until it is cut off: it holds a valid device
  certificate and key. Anyone who can read its storage has the device key.
- **Cannot** sync with a replica that has heard it was revoked (ADR 0040, from 0.20):
  `tidefell device revoke` on the device holding the user key signs a revocation, which every
  peer passes on in its `Hello`, before anything else is exchanged, and keeps for good. A revoked
  device is refused at the handshake, told why, and syncs with no one once it knows.
- **Cannot** get its changes in through another device (from 0.21): its versions are refused by
  every replica that knows of the revocation, however they arrive. What it made before is taken
  over by the replicas that hold it, so those files stay.
- **Cannot** keep what it changed after the user lost it, if the user says when (from 0.21,
  ADR 0040): each replica withdraws what it received from the device since then, by its own
  clock, putting back what each change replaced or moving it to the recycle bin, after showing
  the user the list. The time reaches back at most a folder's retention, and many withdrawals
  wait for the user on each device.
- **On Android the device key is sealed by the Keystore.** The FFI keeps it in a vault the app
  provides (`set_key_vault`), and the app's vault seals it with an AES key held in the Android
  Keystore, which never leaves it: a copy of the app's storage holds only the sealed key. A plain
  key file left by an earlier version moves into the vault the first time the key is used, and
  is overwritten and deleted. On a phone that is unlocked and in the thief's hands, the app can
  still use the key, so revocation (ADR 0040) is what cuts it off.
- **Losing the user key** is worse than losing a device: whoever has it can certify devices for
  the user, and revoke the user's other devices. A revocation's time of loss reaches back at most
  a folder's retention, and one that would withdraw much of a folder waits for the user on each
  device (ADR 0040), so it cannot empty folders unseen. Keep the user key on one trusted
  machine, and back it up offline.

### A share-link holder

Anyone with a share link's URL.

- **Can** download what the link shares, or upload to its folder for an upload link, until it
  expires, is revoked, reaches its download limit or its creator loses access.
- **Cannot** see anything outside what the link names, or guess another link: tokens are 128-bit
  random.
- **Guessing is slowed down.** Each wrong link password or sign-in waits half a second, and after
  three failures the wait doubles each time, up to fifteen minutes, for that link or account.
  Addresses are not counted, since a reverse proxy would hide them, so a stranger who knows a
  username can keep its owner waiting up to fifteen minutes at a time, and no longer.
- **Uploads are bounded.** One uploaded file may have at most `max_upload_bytes` (1 GiB unless
  configured), an upload link can set less, and an upload stops before the drive would go under
  its free-space floor.

### Other users on the same machine

- **Cannot** read the device or user key (0600) or use the control socket (0600, owner only).
  The NixOS module runs `tidefelld` as its own user with a data directory only it can enter
  (0700), and with systemd's sandboxing.
- **Cannot** enter the data directory, or a separate `metadata_dir`, at all: each is created
  readable by its owner alone (0700), and an existing one is made so when the engine starts, which keeps the logs, the databases and
  the control socket, from the moment it is bound, to the owner.
- **Can** read synced files, which are written readable by everyone (0644, or 0755 for
  executables), as most sync tools do; a folder kept private needs a directory others cannot
  enter.

### The supply chain

Dependencies, the build machines and the release process.

- **Protected by** locked dependencies, `cargo-deny`, pinned and checked workflow actions,
  Renovate's waiting period, a release toolchain pinned by version, and SHA-256 files beside
  each release asset.
- **Releases are made only from tags on `main` whose commit passed CI,** on a runner that runs
  nothing else, as its own user with its own caches (`docs/project/release-runner.md`): a pull
  request's job cannot poison what a release builds from.
- **Every asset is signed, keyless.** Sigstore issues the release workflow a certificate naming
  this repository, the workflow file and the tag, and the signature goes in its public log. A
  signature cannot be made by anything but that workflow on a tag, and a checksum beside an
  asset shows it arrived whole while the signature shows the release built it
  (`docs/guide/install.md`). The log is public: it reveals that the repository exists and the
  assets' hashes, which is accepted.
- **Gap: the release runner shares a machine with the pull-request runners,** whose user can
  become root. A separate machine is the boundary that closes this; until then it is a wall
  against accidents and cache poisoning, not against a hostile pull request.

## Encrypted folders

Folders whose replicas include devices that should not read them, such as a storage peer at a
hosting provider, are decided in ADR 0023 and built in Phase 6. Until then every replica of a
folder can read it, and a device that holds a folder should be one its member trusts.

Once built, for an encrypted folder:

- **A storage peer, or whoever runs or breaks into it, can** see the folder's ID, its member
  users and devices, entry IDs, version vectors, which device made each version and when, chunk
  counts and approximate sizes, which versions and entries share chunks (copies, and edits that
  keep most of a file), and when devices connect. It can refuse to store or serve, serve
  old versions, or delete what it holds.
- **It cannot** read paths, names, contents or exact sizes, test whether the folder holds a file
  it knows, or change anything undetected: versions are signed by their authors and chunks are
  authenticated, and members check both.
- **A removed or revoked device keeps** what it could already read. After a member is removed or
  a device revoked, the folder key rotates, so it cannot read what is written afterwards.
- **Losing every member device loses the key,** and with it the data held only on storage peers.
  A phone therefore counts a storage peer's copy as a backup only while another member device
  holds the key.

## Keeping this current

- A pull request that changes what is protected, or how, updates this page; the template asks.
- An ADR that touches keys, membership, the protocols, the web peer or the release process says
  what it changes here.
- Phase 5c step 6 (hardening) works from this page.
