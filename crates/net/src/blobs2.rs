//! `shoal/blobs/2`: encrypted folders' sealed chunks and list blobs. See `specs/blobs-v2.md`.
//!
//! The requester checks what anyone can: a list blob against its ID, and each blob's size. A
//! sealed chunk is opened and checked by whoever holds the folder key, above this crate.

use std::sync::Arc;
use std::time::Duration;

use iroh::protocol::{AcceptError, ProtocolHandler};
use shoal_proto::blobs2::{
    BlobId, BlobsErrorCode, BlobsMessage, MAX_BLOBS_PER_AVAILABILITY, MAX_BLOBS_PER_REQUEST,
    MAX_SEALED_CHUNK,
};
use shoal_proto::identity::DeviceCertificate;
use shoal_proto::{FolderId, Signed};

use crate::backend::EncryptedBackend;
use crate::limits::RateLimiter;
use crate::session::verify_peer;
use crate::traffic::Request;
use crate::transport::{Conn, RecvHalf, SendHalf};
use crate::{frame_io, Error, Result};

/// The outcome for one requested blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobResult {
    /// The peer sent the blob. A list blob has been checked against its ID; a sealed chunk is
    /// checked when it is opened.
    Found(BlobId, Vec<u8>),
    /// The peer does not hold the blob.
    Missing(BlobId),
    /// The peer is at its upload limit and sent none of the blobs that follow; ask again after
    /// this long, or ask another peer. Always last.
    Busy(Duration),
}

/// Whether a requested blob is a list blob, which the requester can check against its ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobKind {
    SealedChunk,
    List,
}

/// Serves blobs/2 requests from an [`EncryptedBackend`].
#[derive(Clone)]
pub(crate) struct Blobs2Protocol {
    pub(crate) folders: Arc<dyn EncryptedBackend>,
    pub(crate) limiter: Arc<RateLimiter>,
}

impl std::fmt::Debug for Blobs2Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Blobs2Protocol").finish_non_exhaustive()
    }
}

impl ProtocolHandler for Blobs2Protocol {
    async fn accept(
        &self,
        conn: iroh::endpoint::Connection,
    ) -> std::result::Result<(), AcceptError> {
        self.serve(Conn::Iroh(conn)).await;
        Ok(())
    }
}

impl Blobs2Protocol {
    /// Serves the requests a peer sends on `conn` until it closes.
    pub(crate) async fn serve(&self, conn: Conn) {
        while let Ok((send, recv)) = conn.accept_bi().await {
            let folders = self.folders.clone();
            let limiter = self.limiter.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_stream(&conn, folders.as_ref(), &limiter, send, recv).await {
                    tracing::debug!(error = %e, "blobs/2 request failed");
                }
            });
        }
    }
}

fn busy(retry_after: Duration) -> BlobsMessage {
    BlobsMessage::Busy {
        retry_after_ms: u32::try_from(retry_after.as_millis()).unwrap_or(u32::MAX),
    }
}

async fn serve_stream(
    conn: &Conn,
    folders: &dyn EncryptedBackend,
    limiter: &RateLimiter,
    mut send: SendHalf,
    mut recv: RecvHalf,
) -> Result<()> {
    // Before the peer is verified, read little and not for long (ADR 0024).
    let hello = frame_io::expect_limited(&mut recv, crate::MAX_UNAUTHENTICATED);
    let BlobsMessage::Hello { certificate } =
        crate::within(crate::HANDSHAKE_TIMEOUT, hello).await?
    else {
        return reject(&mut send, BlobsErrorCode::ProtocolViolation).await;
    };
    let peer = match verify_peer(&certificate, conn) {
        Ok(peer) => peer,
        Err(_) => return reject(&mut send, BlobsErrorCode::NotAMember).await,
    };
    let request = crate::within(crate::REQUEST_TIMEOUT, frame_io::expect(&mut recv)).await?;
    match request {
        BlobsMessage::Request { folder, blobs } => {
            if blobs.len() > MAX_BLOBS_PER_REQUEST {
                return reject(&mut send, BlobsErrorCode::TooManyBlobs).await;
            }
            if !folders.is_member(&folder, &peer) {
                return reject(&mut send, BlobsErrorCode::NotAMember).await;
            }
            for id in blobs {
                if let Err(retry_after) = limiter.upload_busy(conn) {
                    frame_io::write(&mut send, &busy(retry_after)).await?;
                    break;
                }
                let reply = match folders.blob(&folder, &id) {
                    Some(data) => match limiter.upload(conn, data.len()) {
                        Ok(wait) => {
                            if !wait.is_zero() {
                                tokio::time::sleep(wait).await;
                            }
                            BlobsMessage::Blob { id, data }
                        }
                        // Sending more would miss the requester's deadline: stop here.
                        Err(retry_after) => {
                            frame_io::write(&mut send, &busy(retry_after)).await?;
                            break;
                        }
                    },
                    None => BlobsMessage::Missing { id },
                };
                let served = matches!(reply, BlobsMessage::Blob { .. });
                frame_io::write(&mut send, &reply).await?;
                if served {
                    limiter.traffic.chunk_served();
                }
            }
        }
        BlobsMessage::AvailabilityRequest { folder, blobs } => {
            if blobs.len() > MAX_BLOBS_PER_AVAILABILITY {
                return reject(&mut send, BlobsErrorCode::TooManyBlobs).await;
            }
            if !folders.is_member(&folder, &peer) {
                return reject(&mut send, BlobsErrorCode::NotAMember).await;
            }
            let held = folders
                .holds(&folder, &blobs)
                .unwrap_or_else(|| vec![false; blobs.len()]);
            frame_io::write(
                &mut send,
                &BlobsMessage::Availability {
                    bitmap: bitmap(&held),
                },
            )
            .await?;
        }
        BlobsMessage::ReplicaRequest { folder } => {
            if !folders.is_member(&folder, &peer) {
                return reject(&mut send, BlobsErrorCode::NotAMember).await;
            }
            let Some(full) = folders.is_full_replica(&folder) else {
                return reject(&mut send, BlobsErrorCode::UnknownFolder).await;
            };
            frame_io::write(&mut send, &BlobsMessage::Replica { full }).await?;
        }
        _ => return reject(&mut send, BlobsErrorCode::ProtocolViolation).await,
    }
    send.finish().map_err(|e| Error::Stream(e.to_string()))?;
    Ok(())
}

/// One bit per entry, least significant bit first.
fn bitmap(held: &[bool]) -> Vec<u8> {
    let mut bits = vec![0u8; held.len().div_ceil(8)];
    for (i, _) in held.iter().enumerate().filter(|(_, h)| **h) {
        if let Some(byte) = bits.get_mut(i / 8) {
            *byte |= 1 << (i % 8);
        }
    }
    bits
}

async fn reject(send: &mut SendHalf, code: BlobsErrorCode) -> Result<()> {
    frame_io::write(send, &BlobsMessage::Error { code }).await?;
    let _ = send.finish();
    Ok(())
}

async fn open(
    conn: &Conn,
    limiter: &RateLimiter,
    certificate: Signed<DeviceCertificate>,
    request: BlobsMessage,
) -> Result<RecvHalf> {
    limiter
        .request(match &request {
            BlobsMessage::Request { blobs, .. } => Request::Chunks(blobs.len()),
            BlobsMessage::AvailabilityRequest { .. } => Request::Availability,
            _ => Request::Replica,
        })
        .await;
    crate::within(crate::REQUEST_TIMEOUT, async {
        let (mut send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Stream(e.to_string()))?;
        frame_io::write(&mut send, &BlobsMessage::Hello { certificate }).await?;
        frame_io::write(&mut send, &request).await?;
        send.finish().map_err(|e| Error::Stream(e.to_string()))?;
        Ok(recv)
    })
    .await
}

/// Requests blobs, saying of each whether it is a list blob, which is checked against its ID.
pub(crate) async fn fetch_blobs(
    conn: &Conn,
    limiter: &RateLimiter,
    certificate: Signed<DeviceCertificate>,
    folder: FolderId,
    wanted: &[(BlobId, BlobKind)],
) -> Result<Vec<BlobResult>> {
    if wanted.len() > MAX_BLOBS_PER_REQUEST {
        return Err(Error::Protocol("too many blobs in one request".into()));
    }
    let request = BlobsMessage::Request {
        folder,
        blobs: wanted.iter().map(|(id, _)| *id).collect(),
    };
    let mut recv = open(conn, limiter, certificate, request).await?;
    let mut results = Vec::with_capacity(wanted.len());
    for (expected, kind) in wanted {
        let response = crate::within(crate::REQUEST_TIMEOUT, frame_io::expect(&mut recv)).await?;
        match response {
            BlobsMessage::Blob { id, data } if id == *expected => {
                let fits = match kind {
                    BlobKind::SealedChunk => data.len() <= MAX_SEALED_CHUNK,
                    BlobKind::List => {
                        data.len() <= shoal_proto::blobs2::MAX_LIST_BLOB
                            && blake3::hash(&data).as_bytes() == id.as_bytes()
                    }
                };
                if !fits {
                    return Err(Error::Protocol(
                        "a blob that is not what it was asked as".into(),
                    ));
                }
                let wait = limiter.download(conn, data.len());
                results.push(BlobResult::Found(id, data));
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
            }
            BlobsMessage::Busy { retry_after_ms } => {
                results.push(BlobResult::Busy(Duration::from_millis(
                    retry_after_ms.into(),
                )));
                return Ok(results);
            }
            BlobsMessage::Missing { id } if id == *expected => {
                results.push(BlobResult::Missing(id));
            }
            BlobsMessage::Error { code } => return Err(Error::Rejected(format!("{code:?}"))),
            _ => return Err(Error::Protocol("unexpected blob response".into())),
        }
    }
    Ok(results)
}

/// Asks which of `ids` the peer holds, in order.
pub(crate) async fn availability(
    conn: &Conn,
    limiter: &RateLimiter,
    certificate: Signed<DeviceCertificate>,
    folder: FolderId,
    ids: &[BlobId],
) -> Result<Vec<bool>> {
    if ids.len() > MAX_BLOBS_PER_AVAILABILITY {
        return Err(Error::Protocol("too many blobs in one request".into()));
    }
    let request = BlobsMessage::AvailabilityRequest {
        folder,
        blobs: ids.to_vec(),
    };
    let mut recv = open(conn, limiter, certificate, request).await?;
    match crate::within(crate::REQUEST_TIMEOUT, frame_io::expect(&mut recv)).await? {
        BlobsMessage::Availability { bitmap } if bitmap.len() == ids.len().div_ceil(8) => Ok((0
            ..ids.len())
            .map(|i| bitmap.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0))
            .collect()),
        BlobsMessage::Availability { .. } => Err(Error::Protocol(
            "availability bitmap has the wrong length".into(),
        )),
        BlobsMessage::Error { code } => Err(Error::Rejected(format!("{code:?}"))),
        _ => Err(Error::Protocol("unexpected availability response".into())),
    }
}

/// Asks whether the peer is a full replica of `folder`.
pub(crate) async fn replica(
    conn: &Conn,
    limiter: &RateLimiter,
    certificate: Signed<DeviceCertificate>,
    folder: FolderId,
) -> Result<bool> {
    let mut recv = open(
        conn,
        limiter,
        certificate,
        BlobsMessage::ReplicaRequest { folder },
    )
    .await?;
    match crate::within(crate::REQUEST_TIMEOUT, frame_io::expect(&mut recv)).await? {
        BlobsMessage::Replica { full } => Ok(full),
        BlobsMessage::Error { code } => Err(Error::Rejected(format!("{code:?}"))),
        _ => Err(Error::Protocol("unexpected replica response".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmaps_hold_one_bit_per_blob() {
        assert_eq!(bitmap(&[]), Vec::<u8>::new());
        assert_eq!(bitmap(&[true, true, false]), vec![0b011]);
        let mut nine = vec![false; 9];
        nine[8] = true;
        assert_eq!(bitmap(&nine), vec![0, 1]);
    }
}
