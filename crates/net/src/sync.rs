//! `tidefell/sync/1`: plaintext folders' sessions, run by [`crate::session`] over sync/1's
//! messages and a [`Backend`]. See `specs/sync-v1.md`. Manifest requests are sync/1's own.

use std::collections::BTreeSet;
use std::sync::Arc;

use tidefell_proto::identity::{DeviceCertificate, DeviceRevocation};
use tidefell_proto::sync::{EntryVersion, ErrorCode, SyncMessage};
use tidefell_proto::{ContentHash, DeviceId, FolderId, Manifest, Signed};
use tokio::sync::watch;

use crate::backend::Backend;
use crate::limits::RateLimiter;
use crate::session::{Code, Event, Page, Protocol, Side};
use crate::transport::Conn;
use crate::{frame_io, Error, Result};

pub(crate) use crate::session::{verify_peer, Peer};

/// sync/1's messages and items.
#[derive(Debug)]
pub(crate) struct V1;

impl Protocol for V1 {
    type Message = SyncMessage;
    type Item = Signed<EntryVersion>;

    /// sync/1 bounds batches by count only.
    const MAX_BATCH_BYTES: usize = usize::MAX;

    fn hello(
        certificate: Signed<DeviceCertificate>,
        folders: Vec<FolderId>,
        clock_ms: i64,
        revocations: Vec<Signed<DeviceRevocation>>,
    ) -> SyncMessage {
        SyncMessage::Hello {
            certificate,
            folders,
            clock_ms: Some(clock_ms),
            revocations: Some(revocations),
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
        versions: Vec<Signed<EntryVersion>>,
        last: bool,
    ) -> SyncMessage {
        SyncMessage::IndexBatch {
            folder,
            epoch,
            up_to,
            versions,
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
                clock_ms,
                revocations,
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
                versions,
                last,
            } => Event::IndexBatch {
                folder,
                epoch,
                up_to,
                items: versions,
                last,
            },
            SyncMessage::CertificateRequest { folder, device } => {
                Event::CertificateRequest { folder, device }
            }
            SyncMessage::CertificateResponse { certificate } => {
                Event::CertificateResponse(certificate)
            }
            SyncMessage::Error { code } => Event::Error(format!("{code:?}")),
            other => Event::Other(other),
        }
    }

    fn name(message: &SyncMessage) -> &'static str {
        match message {
            SyncMessage::Hello { .. } => "Hello",
            SyncMessage::IndexRequest { .. } => "IndexRequest",
            SyncMessage::IndexBatch { .. } => "IndexBatch",
            SyncMessage::ManifestRequest { .. } => "ManifestRequest",
            SyncMessage::ManifestResponse { .. } => "ManifestResponse",
            SyncMessage::Error { .. } => "Error",
            SyncMessage::CertificateRequest { .. } => "CertificateRequest",
            SyncMessage::CertificateResponse { .. } => "CertificateResponse",
        }
    }
}

/// A [`Backend`] as the session's sync/1 side.
pub(crate) struct V1Side(pub(crate) Arc<dyn Backend>);

impl Side<V1> for V1Side {
    fn certificate(&self) -> Signed<DeviceCertificate> {
        self.0.certificate()
    }

    fn now_ms(&self) -> i64 {
        self.0.now_ms()
    }

    fn folders(&self) -> Vec<FolderId> {
        self.0.folders()
    }

    fn is_member(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool {
        self.0.is_member(folder, peer)
    }

    fn certificate_of(
        &self,
        folder: &FolderId,
        device: DeviceId,
    ) -> Option<Signed<DeviceCertificate>> {
        self.0.certificate_of(folder, device)
    }

    fn learn_certificate(&self, certificate: Signed<DeviceCertificate>) {
        self.0.learn_certificate(certificate);
    }

    fn revocations(&self) -> Vec<Signed<DeviceRevocation>> {
        self.0.revocations()
    }

    fn learn_revocations(&self, revocations: Vec<Signed<DeviceRevocation>>) {
        self.0.learn_revocations(revocations);
    }

    fn is_revoked(&self, certificate: &DeviceCertificate) -> bool {
        self.0.is_revoked(certificate)
    }

    fn unknown_authors(&self, versions: &[Signed<EntryVersion>]) -> Vec<DeviceId> {
        self.0.unknown_authors(versions)
    }

    fn stored_unknown_authors(&self, folder: &FolderId) -> Vec<DeviceId> {
        self.0.stored_unknown_authors(folder)
    }

    /// In sync/1 every peer that shares a folder is a member, so its answer counts.
    fn cannot_vouch(&self, folder: &FolderId, author: DeviceId, _peer: &DeviceCertificate) {
        self.0.cannot_vouch(folder, author);
    }

    fn index_since(
        &self,
        folder: &FolderId,
        epoch: u64,
        since: u64,
        limit: usize,
    ) -> Option<Page<Signed<EntryVersion>>> {
        self.0
            .index_since(folder, epoch, since, limit)
            .map(|page| Page {
                epoch: page.epoch,
                up_to: page.up_to,
                items: page.versions,
            })
    }

    fn apply(&self, folder: &FolderId, peer: DeviceId, versions: Vec<Signed<EntryVersion>>) {
        self.0.apply_versions(folder, peer, versions);
    }

    fn peer_progress(&self, folder: &FolderId, peer: DeviceId) -> (u64, u64) {
        self.0.peer_progress(folder, peer)
    }

    fn set_peer_progress(&self, folder: &FolderId, peer: DeviceId, epoch: u64, up_to: u64) {
        self.0.set_peer_progress(folder, peer, epoch, up_to);
    }

    /// Manifest requests, sync/1's own.
    fn answer_other(&self, request: SyncMessage, shared: &BTreeSet<FolderId>) -> SyncMessage {
        match request {
            SyncMessage::ManifestRequest { folder, content } => {
                if !shared.contains(&folder) {
                    V1::error(Code::NotAMember)
                } else {
                    match self.0.manifest(&folder, &content) {
                        Some(manifest) => SyncMessage::ManifestResponse { manifest },
                        None => SyncMessage::Error {
                            code: ErrorCode::UnknownManifest,
                        },
                    }
                }
            }
            _ => V1::error(Code::ProtocolViolation),
        }
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.0.changes()
    }
}

/// Requests a manifest from a connected peer on a new stream.
pub(crate) async fn request_manifest(
    conn: &Conn,
    limiter: &RateLimiter,
    folder: FolderId,
    content: ContentHash,
) -> Result<Manifest> {
    limiter.request(crate::traffic::Request::Manifest).await;
    crate::within(
        crate::REQUEST_TIMEOUT,
        manifest_exchange(conn, folder, content),
    )
    .await
}

async fn manifest_exchange(
    conn: &Conn,
    folder: FolderId,
    content: ContentHash,
) -> Result<Manifest> {
    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| Error::Stream(e.to_string()))?;
    frame_io::write(&mut send, &SyncMessage::ManifestRequest { folder, content }).await?;
    let _ = send.finish();
    match frame_io::expect(&mut recv).await? {
        SyncMessage::ManifestResponse { manifest } if manifest.content == content => Ok(manifest),
        SyncMessage::ManifestResponse { .. } => {
            Err(Error::Protocol("manifest for different content".into()))
        }
        SyncMessage::Error { code } => Err(Error::Rejected(format!("{code:?}"))),
        other => Err(Error::Protocol(format!(
            "unexpected manifest response: {}",
            V1::name(&other)
        ))),
    }
}

/// Requests the certificate of `device`, an author in `folder`, from a connected peer on a new
/// sync/1 stream (ADR 0024).
pub(crate) async fn request_certificate(
    conn: &Conn,
    limiter: &RateLimiter,
    folder: FolderId,
    device: DeviceId,
) -> Result<Signed<DeviceCertificate>> {
    crate::session::request_certificate::<V1>(conn, limiter, folder, device).await
}

/// The device ID of a peer's endpoint.
pub(crate) fn device_of(conn: &Conn) -> DeviceId {
    conn.remote()
}
