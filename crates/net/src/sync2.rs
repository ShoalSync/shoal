//! `shoal/sync/2`: encrypted folders' sessions, run by [`crate::session`] over sync/2's
//! messages and an [`EncryptedBackend`]. See `specs/sync-v2.md`.

use std::sync::Arc;

use shoal_proto::identity::{DeviceCertificate, DeviceRevocation};
use shoal_proto::sync2::{ErrorCode, Item, SyncMessage, MAX_BATCH_BYTES, MAX_BATCH_ITEMS};
use shoal_proto::{DeviceId, FolderId, Signed};
use tokio::sync::watch;
use tracing::debug;

use crate::backend::{Backend, EncryptedBackend};
use crate::session::{Code, Event, Page, Protocol, Side};

/// sync/2's messages and items.
#[derive(Debug)]
pub(crate) struct V2;

impl Protocol for V2 {
    type Message = SyncMessage;
    type Item = Item;

    const MAX_BATCH_BYTES: usize = MAX_BATCH_BYTES;

    fn hello(
        certificate: Signed<DeviceCertificate>,
        folders: Vec<FolderId>,
        clock_ms: i64,
        revocations: Vec<Signed<DeviceRevocation>>,
    ) -> SyncMessage {
        SyncMessage::Hello {
            certificate,
            folders,
            clock_ms,
            revocations,
        }
    }

    fn index_request(folder: FolderId, epoch: u64, since: u64) -> SyncMessage {
        SyncMessage::IndexRequest {
            folder,
            epoch,
            since,
        }
    }

    fn index_batch(
        folder: FolderId,
        epoch: u64,
        up_to: u64,
        items: Vec<Item>,
        last: bool,
    ) -> SyncMessage {
        SyncMessage::IndexBatch {
            folder,
            epoch,
            up_to,
            items,
            last,
        }
    }

    fn certificate_request(folder: FolderId, device: DeviceId) -> SyncMessage {
        SyncMessage::CertificateRequest { folder, device }
    }

    fn certificate_response(certificate: Signed<DeviceCertificate>) -> SyncMessage {
        SyncMessage::CertificateResponse { certificate }
    }

    fn error(code: Code) -> SyncMessage {
        SyncMessage::Error {
            code: match code {
                Code::NotAMember => ErrorCode::NotAMember,
                Code::UnknownFolder => ErrorCode::UnknownFolder,
                Code::UnknownDevice => ErrorCode::UnknownDevice,
                Code::ProtocolViolation => ErrorCode::ProtocolViolation,
            },
        }
    }

    fn event(message: SyncMessage) -> Event<Self> {
        match message {
            SyncMessage::Hello {
                certificate,
                folders,
                clock_ms,
                revocations,
            } => Event::Hello {
                certificate,
                folders,
                clock_ms: Some(clock_ms),
                revocations: Some(revocations),
            },
            SyncMessage::IndexRequest {
                folder,
                epoch,
                since,
            } => Event::IndexRequest {
                folder,
                epoch,
                since,
            },
            SyncMessage::IndexBatch {
                folder,
                epoch,
                up_to,
                items,
                last,
            } => Event::IndexBatch {
                folder,
                epoch,
                up_to,
                items,
                last,
            },
            SyncMessage::CertificateRequest { folder, device } => {
                Event::CertificateRequest { folder, device }
            }
            SyncMessage::CertificateResponse { certificate } => {
                Event::CertificateResponse(certificate)
            }
            SyncMessage::Error { code } => Event::Error(format!("{code:?}")),
        }
    }

    fn name(message: &SyncMessage) -> &'static str {
        match message {
            SyncMessage::Hello { .. } => "Hello",
            SyncMessage::IndexRequest { .. } => "IndexRequest",
            SyncMessage::IndexBatch { .. } => "IndexBatch",
            SyncMessage::CertificateRequest { .. } => "CertificateRequest",
            SyncMessage::CertificateResponse { .. } => "CertificateResponse",
            SyncMessage::Error { .. } => "Error",
        }
    }

    /// A batch holds at most 512 items and 4 MiB of them.
    #[expect(
        clippy::expect_used,
        reason = "postcard fails to encode only sequences of unknown length, which no message has"
    )]
    fn check(message: &SyncMessage) -> bool {
        match message {
            SyncMessage::IndexBatch { items, .. } => {
                items.len() <= MAX_BATCH_ITEMS
                    && postcard::to_stdvec(items)
                        .expect("in-memory encoding cannot fail")
                        .len()
                        <= MAX_BATCH_BYTES.saturating_add(2)
            }
            SyncMessage::Hello { revocations, .. } => {
                revocations.len() <= shoal_proto::sync2::MAX_HELLO_REVOCATIONS
            }
            _ => true,
        }
    }
}

/// A device's [`Backend`] and its [`EncryptedBackend`], as the session's sync/2 side.
pub(crate) struct V2Side {
    pub(crate) device: Arc<dyn Backend>,
    pub(crate) folders: Arc<dyn EncryptedBackend>,
}

impl Side<V2> for V2Side {
    fn certificate(&self) -> Signed<DeviceCertificate> {
        self.device.certificate()
    }

    fn now_ms(&self) -> i64 {
        self.device.now_ms()
    }

    fn folders(&self) -> Vec<FolderId> {
        self.folders.folders()
    }

    fn is_member(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool {
        self.folders.is_member(folder, peer)
    }

    fn certificate_of(
        &self,
        folder: &FolderId,
        device: DeviceId,
    ) -> Option<Signed<DeviceCertificate>> {
        self.device.certificate_of(folder, device)
    }

    fn learn_certificate(&self, certificate: Signed<DeviceCertificate>) {
        self.device.learn_certificate(certificate);
    }

    fn revocations(&self) -> Vec<Signed<DeviceRevocation>> {
        self.device.revocations()
    }

    fn learn_revocations(&self, revocations: Vec<Signed<DeviceRevocation>>) {
        self.device.learn_revocations(revocations);
    }

    fn is_revoked(&self, certificate: &DeviceCertificate) -> bool {
        self.device.is_revoked(certificate)
    }

    fn unknown_authors(&self, items: &[Item]) -> Vec<DeviceId> {
        self.folders.unknown_authors(items)
    }

    fn stored_unknown_authors(&self, folder: &FolderId) -> Vec<DeviceId> {
        self.folders.stored_unknown_authors(folder)
    }

    /// A storage peer's member list is its operator's, so its answer counts for nothing
    /// (ADR 0023): the items stay held, as for an unanswered request, and nothing is taken over.
    fn cannot_vouch(&self, folder: &FolderId, author: DeviceId, peer: &DeviceCertificate) {
        if self.folders.is_storage_peer(folder, peer) {
            debug!(%folder, %author, peer = %peer.device, "a storage peer cannot vouch; ignored");
            return;
        }
        self.folders.cannot_vouch(folder, author);
    }

    fn counts_as_answer(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool {
        !self.folders.is_storage_peer(folder, peer)
    }

    fn index_since(
        &self,
        folder: &FolderId,
        epoch: u64,
        since: u64,
        limit: usize,
    ) -> Option<Page<Item>> {
        self.folders
            .index_since(folder, epoch, since, limit)
            .map(|page| Page {
                epoch: page.epoch,
                up_to: page.up_to,
                items: page.items,
            })
    }

    fn apply(&self, folder: &FolderId, peer: DeviceId, items: Vec<Item>) {
        self.folders.apply_items(folder, peer, items);
    }

    fn peer_progress(&self, folder: &FolderId, peer: DeviceId) -> (u64, u64) {
        self.folders.peer_progress(folder, peer)
    }

    fn set_peer_progress(&self, folder: &FolderId, peer: DeviceId, epoch: u64, up_to: u64) {
        self.folders.set_peer_progress(folder, peer, epoch, up_to);
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.device.changes()
    }
}
