//! The storage and policy interface a [`crate::Node`] needs. Implemented by `engine`.

use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::sync::EntryVersion;
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
