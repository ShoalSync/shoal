//! `tidefell/storage/1`: members' priorities for what a storage peer keeps of an encrypted
//! folder. See `specs/storage-v1.md`.

use std::sync::Arc;

use iroh::protocol::{AcceptError, ProtocolHandler};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::storage::{Hint, StorageErrorCode, StorageMessage, MAX_HINTS};
use tidefell_proto::{FolderId, Signed};

use crate::backend::EncryptedBackend;
use crate::session::verify_peer;
use crate::transport::{Conn, SendHalf};
use crate::{frame_io, Error, Result};

/// Serves storage/1 requests from an [`EncryptedBackend`].
#[derive(Clone)]
pub(crate) struct StorageProtocol {
    pub(crate) folders: Arc<dyn EncryptedBackend>,
}

impl std::fmt::Debug for StorageProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageProtocol").finish_non_exhaustive()
    }
}

impl ProtocolHandler for StorageProtocol {
    async fn accept(
        &self,
        conn: iroh::endpoint::Connection,
    ) -> std::result::Result<(), AcceptError> {
        self.serve(Conn::Iroh(conn)).await;
        Ok(())
    }
}

impl StorageProtocol {
    /// Serves the requests a peer sends on `conn` until it closes.
    pub(crate) async fn serve(&self, conn: Conn) {
        while let Ok((send, recv)) = conn.accept_bi().await {
            let folders = self.folders.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_stream(&conn, folders.as_ref(), send, recv).await {
                    tracing::debug!(error = %e, "storage/1 request failed");
                }
            });
        }
    }
}

async fn serve_stream(
    conn: &Conn,
    folders: &dyn EncryptedBackend,
    mut send: SendHalf,
    mut recv: crate::transport::RecvHalf,
) -> Result<()> {
    // Before the peer is verified, read little and not for long (ADR 0024).
    let hello = frame_io::expect_limited(&mut recv, crate::MAX_UNAUTHENTICATED);
    let StorageMessage::Hello { certificate } =
        crate::within(crate::HANDSHAKE_TIMEOUT, hello).await?
    else {
        return reject(&mut send, StorageErrorCode::ProtocolViolation).await;
    };
    let peer = match verify_peer(&certificate, conn) {
        Ok(peer) => peer,
        Err(_) => return reject(&mut send, StorageErrorCode::NotAMember).await,
    };
    let request = crate::within(crate::REQUEST_TIMEOUT, frame_io::expect(&mut recv)).await?;
    match request {
        StorageMessage::Priorities {
            folder,
            issued_ms,
            hints,
        } => {
            if hints.len() > MAX_HINTS {
                return reject(&mut send, StorageErrorCode::TooManyHints).await;
            }
            if !folders.is_member(&folder, &peer) {
                return reject(&mut send, StorageErrorCode::NotAMember).await;
            }
            if !folders.set_priorities(&folder, peer.device, issued_ms, &hints) {
                return reject(&mut send, StorageErrorCode::NotStored).await;
            }
            frame_io::write(&mut send, &StorageMessage::Accepted).await?;
        }
        StorageMessage::UsageRequest { folder } => {
            if !folders.is_member(&folder, &peer) {
                return reject(&mut send, StorageErrorCode::NotAMember).await;
            }
            let Some((quota_bytes, held_bytes, room_bytes)) = folders.storage_usage(&folder) else {
                return reject(&mut send, StorageErrorCode::NotStored).await;
            };
            let usage = StorageMessage::Usage {
                quota_bytes,
                held_bytes,
                room_bytes,
            };
            frame_io::write(&mut send, &usage).await?;
        }
        StorageMessage::Drop { folder, below } => {
            if !folders.is_member(&folder, &peer) {
                return reject(&mut send, StorageErrorCode::NotAMember).await;
            }
            let Some((chunks, bytes)) = folders.storage_drop(&folder, below) else {
                return reject(&mut send, StorageErrorCode::NotStored).await;
            };
            frame_io::write(&mut send, &StorageMessage::Dropped { chunks, bytes }).await?;
        }
        _ => return reject(&mut send, StorageErrorCode::ProtocolViolation).await,
    }
    send.finish().map_err(|e| Error::Stream(e.to_string()))?;
    Ok(())
}

async fn reject(send: &mut SendHalf, code: StorageErrorCode) -> Result<()> {
    frame_io::write(send, &StorageMessage::Error { code }).await?;
    let _ = send.finish();
    Ok(())
}

/// Sends `request` on a stream of its own, after the `Hello`, and reads the one answer.
async fn ask(
    conn: &Conn,
    certificate: Signed<DeviceCertificate>,
    request: StorageMessage,
) -> Result<StorageMessage> {
    let mut recv = crate::within(crate::REQUEST_TIMEOUT, async {
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Stream(e.to_string()))?;
        frame_io::write(&mut send, &StorageMessage::Hello { certificate }).await?;
        frame_io::write(&mut send, &request).await?;
        send.finish().map_err(|e| Error::Stream(e.to_string()))?;
        Ok::<_, Error>(recv)
    })
    .await?;
    match crate::within(crate::REQUEST_TIMEOUT, frame_io::expect(&mut recv)).await? {
        StorageMessage::Error { code } => Err(Error::Rejected(format!("{code:?}"))),
        answer => Ok(answer),
    }
}

/// Gives a storage peer the priorities of up to [`MAX_HINTS`] of `folder`'s entries.
pub(crate) async fn send_priorities(
    conn: &Conn,
    certificate: Signed<DeviceCertificate>,
    folder: FolderId,
    issued_ms: i64,
    hints: Vec<Hint>,
) -> Result<()> {
    if hints.len() > MAX_HINTS {
        return Err(Error::Protocol("too many hints in one request".into()));
    }
    let request = StorageMessage::Priorities {
        folder,
        issued_ms,
        hints,
    };
    match ask(conn, certificate, request).await? {
        StorageMessage::Accepted => Ok(()),
        _ => Err(Error::Protocol("unexpected storage response".into())),
    }
}

/// Asks a storage peer its quota for `folder`, what it holds and the room it has left.
pub(crate) async fn usage(
    conn: &Conn,
    certificate: Signed<DeviceCertificate>,
    folder: FolderId,
) -> Result<(u64, u64, u64)> {
    match ask(conn, certificate, StorageMessage::UsageRequest { folder }).await? {
        StorageMessage::Usage {
            quota_bytes,
            held_bytes,
            room_bytes,
        } => Ok((quota_bytes, held_bytes, room_bytes)),
        _ => Err(Error::Protocol("unexpected storage response".into())),
    }
}

/// Asks a storage peer to forget what it holds of `folder`'s entries ranked below `below`.
pub(crate) async fn drop_below(
    conn: &Conn,
    certificate: Signed<DeviceCertificate>,
    folder: FolderId,
    below: u8,
) -> Result<(u64, u64)> {
    match ask(conn, certificate, StorageMessage::Drop { folder, below }).await? {
        StorageMessage::Dropped { chunks, bytes } => Ok((chunks, bytes)),
        _ => Err(Error::Protocol("unexpected storage response".into())),
    }
}
