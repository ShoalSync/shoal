//! `tidefell/blobs/1`: chunk and availability requests. See `specs/blobs-v1.md`.

use std::sync::Arc;
use std::time::Duration;

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use tidefell_proto::blobs::{BlobsErrorCode, BlobsMessage, MAX_CHUNKS_PER_REQUEST};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::{ChunkHash, ContentHash, FolderId, Signed};

use crate::backend::Backend;
use crate::limits::RateLimiter;
use crate::sync::verify_peer;
use crate::{frame_io, Error, Result};

/// The outcome for one requested chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkResult {
    /// The peer sent the chunk and it matched its hash.
    Found(ChunkHash, Vec<u8>),
    /// The peer does not hold the chunk.
    Missing(ChunkHash),
    /// The peer is at its upload limit and sent none of the chunks that follow in the request;
    /// ask again after this long, or ask another peer (ADR 0029). Always last.
    Busy(Duration),
}

/// Serves blobs/1 requests from a [`Backend`].
#[derive(Clone)]
pub(crate) struct BlobsProtocol {
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) limiter: Arc<RateLimiter>,
}

impl std::fmt::Debug for BlobsProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlobsProtocol").finish_non_exhaustive()
    }
}

impl ProtocolHandler for BlobsProtocol {
    async fn accept(&self, conn: Connection) -> std::result::Result<(), AcceptError> {
        while let Ok((send, recv)) = conn.accept_bi().await {
            let backend = self.backend.clone();
            let limiter = self.limiter.clone();
            let conn = conn.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_stream(&conn, backend.as_ref(), &limiter, send, recv).await {
                    tracing::debug!(error = %e, "blobs request failed");
                }
            });
        }
        Ok(())
    }
}

async fn serve_stream(
    conn: &Connection,
    backend: &dyn Backend,
    limiter: &RateLimiter,
    mut send: SendStream,
    mut recv: RecvStream,
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
        BlobsMessage::Request { folder, chunks } => {
            if chunks.len() > MAX_CHUNKS_PER_REQUEST {
                return reject(&mut send, BlobsErrorCode::TooManyChunks).await;
            }
            if !backend.is_member(&folder, &peer) {
                return reject(&mut send, BlobsErrorCode::NotAMember).await;
            }
            for hash in chunks {
                if let Err(retry_after) = limiter.upload_busy(conn) {
                    let retry_after_ms = u32::try_from(retry_after.as_millis()).unwrap_or(u32::MAX);
                    frame_io::write(&mut send, &BlobsMessage::Busy { retry_after_ms }).await?;
                    break;
                }
                let reply = match backend.chunk(&folder, &hash) {
                    Some(data) => match limiter.upload(conn, data.len()) {
                        Ok(wait) => {
                            if !wait.is_zero() {
                                tokio::time::sleep(wait).await;
                            }
                            BlobsMessage::Chunk { hash, data }
                        }
                        // Sending more would miss the requester's deadline: stop here.
                        Err(retry_after) => {
                            let retry_after_ms =
                                u32::try_from(retry_after.as_millis()).unwrap_or(u32::MAX);
                            frame_io::write(&mut send, &BlobsMessage::Busy { retry_after_ms })
                                .await?;
                            break;
                        }
                    },
                    None => BlobsMessage::Missing { hash },
                };
                frame_io::write(&mut send, &reply).await?;
            }
        }
        BlobsMessage::AvailabilityRequest { folder, content } => {
            if !backend.is_member(&folder, &peer) {
                return reject(&mut send, BlobsErrorCode::NotAMember).await;
            }
            let (chunk_count, bitmap) = backend
                .availability(&folder, &content)
                .unwrap_or((0, Vec::new()));
            frame_io::write(
                &mut send,
                &BlobsMessage::Availability {
                    content,
                    chunk_count,
                    bitmap,
                },
            )
            .await?;
        }
        BlobsMessage::ReplicaRequest { folder } => {
            if !backend.is_member(&folder, &peer) {
                return reject(&mut send, BlobsErrorCode::NotAMember).await;
            }
            let Some(full) = backend.is_full_replica(&folder) else {
                return reject(&mut send, BlobsErrorCode::UnknownFolder).await;
            };
            frame_io::write(&mut send, &BlobsMessage::Replica { full }).await?;
        }
        _ => return reject(&mut send, BlobsErrorCode::ProtocolViolation).await,
    }
    send.finish().map_err(|e| Error::Stream(e.to_string()))?;
    Ok(())
}

async fn reject(send: &mut SendStream, code: BlobsErrorCode) -> Result<()> {
    frame_io::write(send, &BlobsMessage::Error { code }).await?;
    let _ = send.finish();
    Ok(())
}

async fn open(
    conn: &Connection,
    certificate: Signed<DeviceCertificate>,
    request: BlobsMessage,
) -> Result<RecvStream> {
    crate::within(
        crate::REQUEST_TIMEOUT,
        open_stream(conn, certificate, request),
    )
    .await
}

async fn open_stream(
    conn: &Connection,
    certificate: Signed<DeviceCertificate>,
    request: BlobsMessage,
) -> Result<RecvStream> {
    let (mut send, recv) = conn
        .open_bi()
        .await
        .map_err(|e| Error::Stream(e.to_string()))?;
    frame_io::write(&mut send, &BlobsMessage::Hello { certificate }).await?;
    frame_io::write(&mut send, &request).await?;
    send.finish().map_err(|e| Error::Stream(e.to_string()))?;
    Ok(recv)
}

/// Requests chunks. Every returned chunk has been checked against its hash.
pub(crate) async fn fetch_chunks(
    conn: &Connection,
    limiter: &RateLimiter,
    certificate: Signed<DeviceCertificate>,
    folder: FolderId,
    chunks: &[ChunkHash],
) -> Result<Vec<ChunkResult>> {
    if chunks.len() > MAX_CHUNKS_PER_REQUEST {
        return Err(Error::Protocol("too many chunks in one request".into()));
    }
    let mut recv = open(
        conn,
        certificate,
        BlobsMessage::Request {
            folder,
            chunks: chunks.to_vec(),
        },
    )
    .await?;
    let mut results = Vec::with_capacity(chunks.len());
    for expected in chunks {
        // A transfer may be long, but no chunk may take longer than the deadline.
        let response = crate::within(crate::REQUEST_TIMEOUT, frame_io::expect(&mut recv)).await?;
        match response {
            BlobsMessage::Chunk { hash, data } if hash == *expected => {
                if blake3::hash(&data).as_bytes() != hash.as_bytes() {
                    return Err(Error::Corrupt(hash));
                }
                // Waiting before the next read lets flow control slow the sender down.
                let wait = limiter.download(conn, data.len());
                results.push(ChunkResult::Found(hash, data));
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
            }
            BlobsMessage::Busy { retry_after_ms } => {
                results.push(ChunkResult::Busy(Duration::from_millis(
                    retry_after_ms.into(),
                )));
                return Ok(results);
            }
            BlobsMessage::Missing { hash } if hash == *expected => {
                results.push(ChunkResult::Missing(hash));
            }
            BlobsMessage::Error { code } => return Err(Error::Rejected(format!("{code:?}"))),
            _ => return Err(Error::Protocol("unexpected chunk response".into())),
        }
    }
    Ok(results)
}

/// Asks which chunks of `content` the peer holds. Returns `(chunk_count, bitmap)`.
pub(crate) async fn availability(
    conn: &Connection,
    certificate: Signed<DeviceCertificate>,
    folder: FolderId,
    content: ContentHash,
) -> Result<(u32, Vec<u8>)> {
    let mut recv = open(
        conn,
        certificate,
        BlobsMessage::AvailabilityRequest { folder, content },
    )
    .await?;
    let response = crate::within(crate::REQUEST_TIMEOUT, frame_io::expect(&mut recv)).await?;
    match response {
        BlobsMessage::Availability {
            content: got,
            chunk_count,
            bitmap,
        } if got == content => {
            if bitmap.len() != (chunk_count as usize).div_ceil(8) {
                return Err(Error::Protocol(
                    "availability bitmap has the wrong length".into(),
                ));
            }
            Ok((chunk_count, bitmap))
        }
        BlobsMessage::Error { code } => Err(Error::Rejected(format!("{code:?}"))),
        _ => Err(Error::Protocol("unexpected availability response".into())),
    }
}

/// Asks whether the peer is a full replica of `folder` (ADR 0022).
pub(crate) async fn replica(
    conn: &Connection,
    certificate: Signed<DeviceCertificate>,
    folder: FolderId,
) -> Result<bool> {
    let mut recv = open(conn, certificate, BlobsMessage::ReplicaRequest { folder }).await?;
    let response = crate::within(crate::REQUEST_TIMEOUT, frame_io::expect(&mut recv)).await?;
    match response {
        BlobsMessage::Replica { full } => Ok(full),
        BlobsMessage::Error { code } => Err(Error::Rejected(format!("{code:?}"))),
        _ => Err(Error::Protocol("unexpected replica response".into())),
    }
}
