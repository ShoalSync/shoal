# Glossary

Every Tidefell repository uses these terms in the sense defined here. Code identifiers follow
the same names (`Folder`, `Replica`, `Tombstone`, ...). If you need a new term, add it here in
the same pull request that introduces it.

| Term | Meaning |
| --- | --- |
| **Peer** | Any running Tidefell node: daemon, desktop, Android app or web peer. All peers speak the same protocols. |
| **Device** | A peer's durable identity: an Ed25519 key pair generated on first run. A device belongs to exactly one user. `DeviceId` is the public key, and is also the device's iroh endpoint ID. |
| **User** | A person's identity: an Ed25519 key pair that signs device certificates. `UserId` is the public key. Folder membership is granted to users, not devices. |
| **Folder** | A directory tree shared between a set of devices. Identified by a random 32-byte `FolderId`. The unit of membership and sync. |
| **Member** | A user allowed to sync a folder. A device may sync the folder if it presents a device certificate from a member. |
| **Device certificate** | A statement "this device belongs to this user", signed by the user's key. Presented in `Hello` on every connection. |
| **Replica** | A device's copy of a folder. |
| **Full replica** | A replica that stores every chunk of every live file, and keeps recycle-bin contents until retention expires. Typically a NAS or server. |
| **Partial replica** | A replica that stores all metadata but only some chunks. Missing content is fetched on demand. |
| **Placeholder** | A file that is visible in a partial replica's file system but whose content is not stored locally. Opening it fetches the content on demand. |
| **Pin** | A file or directory a partial replica keeps on disk (ADR 0029): fetched in the background, kept up to date and never evicted, and not counted against the budget. Pins are per device. Not to be confused with a file *held open*, which is only kept while in use. |
| **Materialised** | A partial replica's file whose content has been fetched and written to disk. Eviction turns it back into a placeholder. |
| **Mount point** | The directory where `tidefelld` shows a folder through FUSE. Opening a placeholder there fetches it; the folder's own directory is then only backing storage. |
| **Chunk** | A contiguous piece of file content produced by FastCDC. Identified by its BLAKE3 hash (`ChunkHash`). Chunks are content-addressed and deduplicated. |
| **Manifest** | The ordered list of chunks (hash and length) that make up one version of a file's content. |
| **Content hash** | BLAKE3 hash of a file's full content. Identifies a manifest. |
| **Entry** | One path in a folder's tree: a file, directory or symlink. Identified by a stable `EntryId`. |
| **Version** | The state of an entry after one change: content, metadata and the version vector at the time. |
| **Version vector** | A map from device to change counter, attached to every version of an entry. Used only to decide whether two versions of that entry are ordered or concurrent. |
| **Conflict** | Two concurrent versions of the same entry. Every replica shows the same winner (edits win over deletes, then the later modification); the losing version is kept as a conflict copy until the user resolves the conflict (ADR 0029). |
| **Conflict copy** | A losing concurrent version, kept until the conflict is resolved. Not written to disk. One with the same content as the winner, or a delete an edit beat, needs no decision. |
| **Resolve** | Deciding a conflict: keep one version, which sends the others to every recycle bin, or keep both, which makes each other version a file of its own. |
| **Index** | A replica's record of the versions it holds for a folder, each numbered with an index sequence. |
| **Index sequence** | A replica-local, increasing number assigned to each version when it is stored. Peers ask for "everything after sequence N". Unrelated to version vectors. |
| **Epoch** | A random identifier of one replica's index. A new epoch means the index was rebuilt and peers must re-fetch it from the start. |
| **Tombstone** | A version recording that an entry was deleted. Propagates the delete to other replicas. |
| **Recycle bin** | Where a deleted entry's last live version is kept until its retention period expires. |
| **Share link** | A web peer URL that gives someone without an account read-only access to one file or directory. It can expire, need a password or limit downloads, and can be revoked. A directory link can instead be *upload-only*: visitors add files and see none (ADR 0029). See ADR 0019. |
| **Retention period** | How long the recycle bin keeps deleted entries. Chosen at first setup; can be changed later. |
| **Block store** | A replica's on-disk, content-addressed chunk storage. |
| **Availability** | Which chunks of a manifest a peer holds, expressed as a bitmap over the manifest's chunk list. |
| **Eviction** | Removing chunks from a partial replica's block store to stay within its size budget. Evicted files become placeholders. |
| **Confirmed copy** | A full replica, other than this device, that has reported holding every chunk of a file this device made. Partial replicas evict such a file only once it has one (ADR 0022). |
| **Swarm fetch** | Downloading one file's chunks from several peers at once. |
| **Rarest-first** | Fetch ordering that requests the chunks held by the fewest peers first. |
| **ALPN** | The QUIC protocol identifier negotiated per connection, e.g. `tidefell/sync/1`. One spec per ALPN. |
| **Relay** | An iroh relay server used when direct connections between peers fail. |
| **Address lookup** | Resolving a `DeviceId` to current network addresses, via DNS/pkarr or local discovery. |
| **Web peer** | A peer that also serves the browser UI and manages user accounts and sessions. |
| **Web account** | A username and password on one web peer, linked to a user. It sees exactly the folders that user is a member of. |
| **Control socket** | `<data_dir>/control.sock`, where a daemon serves its REST API to local tools such as `tidefell`. |
| **Signed envelope** | A wire message wrapped with the signer's public key and an Ed25519 signature. |
| **Pairing** | Adding a new device to a user over the network with `pair/1`: a device holding the user key certifies the new device after the person confirms a code on both. |
| **Pairing code** | What a new device shows to be paired: its device ID, a one-time secret and its addresses. Shown as a QR code or text. Expires after ten minutes or one use. |
| **Approver** | The device that holds the user key and certifies a new device during pairing. |
| **Confirmation code** | Six digits derived from the pairing secret and both device IDs, shown on both devices during pairing so the person can check they match. |
