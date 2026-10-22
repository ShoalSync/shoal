//! The storage and policy interface a [`crate::Node`] needs. Implemented by `engine`.

use tidefell_proto::blobs2::BlobId;
use tidefell_proto::identity::{DeviceCertificate, DeviceRevocation};
use tidefell_proto::sync::EntryVersion;
use tidefell_proto::sync2::Item;
use tidefell_proto::{ChunkHash, ContentHash, DeviceId, FolderId, Manifest, Signed};
use tokio::sync::watch;

/// A page of a folder's index, for an `IndexBatch`.
#[derive(Debug, Clone, Default)]
pub struct IndexPage {
    /// The index's epoch.
    pub epoch: u64,
    /// Highest sequence covered by `versions`.
    pub up_to: u64,
    /// Versions after the requested sequence, in sequence order.
    pub versions: Vec<Signed<EntryVersion>>,
}

/// Everything a node needs from local storage and policy.
///
/// Methods are synchronous and should be quick; they are called from async tasks.
pub trait Backend: Send + Sync + 'static {
    /// This device's certificate, presented to peers.
    fn certificate(&self) -> Signed<DeviceCertificate>;

    /// This device's time, in milliseconds since the Unix epoch, as it tells peers in the
    /// handshake (ADR 0030): the system's unless a simulation skews it (ADR 0036).
    fn now_ms(&self) -> i64 {
        crate::now_ms()
    }

    /// Folders this device syncs, offered when dialing a peer.
    fn folders(&self) -> Vec<FolderId>;

    /// True if `peer` (already verified) may sync `folder`.
    fn is_member(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool;

    /// The certificate of `device`, if this backend holds one and `device`'s user is a member of
    /// `folder` (ADR 0024). Asked only by peers that share the folder.
    fn certificate_of(
        &self,
        _folder: &FolderId,
        _device: DeviceId,
    ) -> Option<Signed<DeviceCertificate>> {
        None
    }

    /// Keeps a device's certificate: a peer's at the handshake, or an author's fetched to vouch
    /// for it (ADR 0024). The signature has been checked.
    fn learn_certificate(&self, _certificate: Signed<DeviceCertificate>) {}

    /// The device revocations to tell peers of (ADR 0040).
    fn revocations(&self) -> Vec<Signed<DeviceRevocation>> {
        Vec::new()
    }

    /// Keeps revocations a peer told of. Their signatures have been checked.
    fn learn_revocations(&self, _revocations: Vec<Signed<DeviceRevocation>>) {}

    /// Whether the device `certificate` is for is revoked: such a peer is refused, and this
    /// device, if it is its own, syncs with no one.
    fn is_revoked(&self, _certificate: &DeviceCertificate) -> bool {
        false
    }

    /// The authors of `versions` whose certificates this backend lacks, each once: the session
    /// asks the peer that sent them for those before applying them (ADR 0024).
    fn unknown_authors(&self, _versions: &[Signed<EntryVersion>]) -> Vec<DeviceId> {
        Vec::new()
    }

    /// Authors of versions this backend stores in `folder` whose certificates it lacks, a few
    /// at a time: each session asks its peer for them (ADR 0024).
    fn stored_unknown_authors(&self, _folder: &FolderId) -> Vec<DeviceId> {
        Vec::new()
    }

    /// A peer that shares `folder` answered it cannot vouch for `author` (ADR 0024).
    fn cannot_vouch(&self, _folder: &FolderId, _author: DeviceId) {}

    /// At most `limit` index entries of `folder` after `since`, from the start if `epoch` does
    /// not match. `up_to` is the sequence of the last entry returned if more follow, otherwise
    /// the index's current sequence. `None` if the folder is unknown.
    fn index_since(
        &self,
        folder: &FolderId,
        epoch: u64,
        since: u64,
        limit: usize,
    ) -> Option<IndexPage>;

    /// Applies versions received from `peer`. Invalid versions are skipped by the backend.
    fn apply_versions(
        &self,
        folder: &FolderId,
        peer: DeviceId,
        versions: Vec<Signed<EntryVersion>>,
    );

    /// The `(epoch, sequence)` last received from `peer` for `folder`, or `(0, 0)`.
    fn peer_progress(&self, folder: &FolderId, peer: DeviceId) -> (u64, u64);

    /// Records progress after a complete index response from `peer`.
    fn set_peer_progress(&self, folder: &FolderId, peer: DeviceId, epoch: u64, up_to: u64);

    /// The manifest of `content` in `folder`, if known.
    fn manifest(&self, folder: &FolderId, content: &ContentHash) -> Option<Manifest>;

    /// A chunk's bytes, if stored and referenced by `folder`.
    fn chunk(&self, folder: &FolderId, hash: &ChunkHash) -> Option<Vec<u8>>;

    /// `(chunk_count, bitmap)` for `content` in `folder`, if the content is known.
    fn availability(&self, folder: &FolderId, content: &ContentHash) -> Option<(u32, Vec<u8>)>;

    /// Whether this device is a full replica of `folder`; `None` if the folder is unknown.
    fn is_full_replica(&self, folder: &FolderId) -> Option<bool>;

    /// A counter that changes whenever local index entries are added, so sessions can push
    /// new versions to peers.
    fn changes(&self) -> watch::Receiver<u64>;
}

/// A page of an encrypted folder's index, for a sync/2 `IndexBatch`.
#[derive(Debug, Clone, Default)]
pub struct ItemPage {
    /// The index's epoch.
    pub epoch: u64,
    /// Highest sequence covered by `items`.
    pub up_to: u64,
    /// Items after the requested sequence, in sequence order.
    pub items: Vec<Item>,
}

/// What a node needs for encrypted folders (`tidefell/sync/2`, `tidefell/blobs/2`), beside a
/// [`Backend`], which still answers for the device: its certificate, clock, revocations and the
/// certificates it holds. Implemented by `engine` on member devices, and by a storage peer.
///
/// Methods are synchronous and should be quick; they are called from async tasks.
pub trait EncryptedBackend: Send + Sync + 'static {
    /// Encrypted folders this device syncs, offered when dialing a peer.
    fn folders(&self) -> Vec<FolderId>;

    /// True if `peer` (already verified) may sync `folder`: a member device, or a storage peer
    /// this device lists for it; on a storage peer, a device of a member user its operator
    /// listed.
    fn is_member(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool;

    /// True if `peer` is a storage peer of `folder`: its user is not a member. Its answer that it
    /// cannot vouch for an author counts for nothing (ADR 0023).
    fn is_storage_peer(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool;

    /// The devices whose certificates this backend lacks among the signers of `items`, each
    /// once: asked for before the items are applied (ADR 0024).
    fn unknown_authors(&self, _items: &[Item]) -> Vec<DeviceId> {
        Vec::new()
    }

    /// Signers of items stored in `folder` whose certificates this backend lacks, a few at a
    /// time.
    fn stored_unknown_authors(&self, _folder: &FolderId) -> Vec<DeviceId> {
        Vec::new()
    }

    /// A member peer sharing `folder` answered it cannot vouch for `author`.
    fn cannot_vouch(&self, _folder: &FolderId, _author: DeviceId) {}

    /// At most `limit` items of `folder`'s index after `since`, as [`Backend::index_since`].
    fn index_since(
        &self,
        folder: &FolderId,
        epoch: u64,
        since: u64,
        limit: usize,
    ) -> Option<ItemPage>;

    /// Applies items received from `peer`. Invalid items are skipped by the backend.
    fn apply_items(&self, folder: &FolderId, peer: DeviceId, items: Vec<Item>);

    /// The `(epoch, sequence)` last received from `peer` for `folder`, or `(0, 0)`.
    fn peer_progress(&self, folder: &FolderId, peer: DeviceId) -> (u64, u64);

    /// Records progress after a complete index response from `peer`.
    fn set_peer_progress(&self, folder: &FolderId, peer: DeviceId, epoch: u64, up_to: u64);

    /// A sealed chunk or list blob of `folder`, if held and referenced there.
    fn blob(&self, folder: &FolderId, id: &BlobId) -> Option<Vec<u8>>;

    /// Which of `ids` this backend holds in `folder`, in order; `None` if the folder is unknown.
    fn holds(&self, folder: &FolderId, ids: &[BlobId]) -> Option<Vec<bool>>;

    /// Whether this device is a full replica of `folder`; `None` if the folder is unknown.
    fn is_full_replica(&self, folder: &FolderId) -> Option<bool>;

    /// Records the priorities member device `from` gave `folder`'s entries at `issued_ms`
    /// (`specs/storage-v1.md`); false if this device does not store the folder.
    fn set_priorities(
        &self,
        folder: &FolderId,
        from: DeviceId,
        issued_ms: i64,
        hints: &[tidefell_proto::storage::Hint],
    ) -> bool {
        let _ = (folder, from, issued_ms, hints);
        false
    }

    /// The stored `folder`'s quota, the sealed bytes held of it and the room left for it;
    /// `None` if this device does not store it.
    fn storage_usage(&self, folder: &FolderId) -> Option<(u64, u64, u64)> {
        let _ = folder;
        None
    }

    /// Forgets what is held of the stored `folder`'s entries ranked below `below`, as a member
    /// asked; the chunks and sealed bytes removed, or `None` if this device does not store it.
    fn storage_drop(&self, folder: &FolderId, below: u8) -> Option<(u64, u64)> {
        let _ = (folder, below);
        None
    }
}
