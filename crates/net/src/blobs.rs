//! `tidefell/blobs/1`: chunk and availability requests. See `specs/blobs-v1.md`.

use std::sync::Arc;

use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use tidefell_proto::blobs::{BlobsErrorCode, BlobsMessage, MAX_CHUNKS_PER_REQUEST};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::{ChunkHash, ContentHash, FolderId, Signed};

use crate::backend::Backend;
use crate::sync::verify_peer;
use crate::{frame_io, Error, Result};

/// The outcome for one requested chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkResult {
    /// The peer sent the chunk and it matched its hash.
    Found(ChunkHash, Vec<u8>),
    /// The peer does not hold the chunk.
    Missing(ChunkHash),
}

/// Serves blobs/1 requests from a [`Backend`].
#[derive(Clone)]
pub(crate) struct BlobsProtocol {
    pub(crate) backend: Arc<dyn Backend>,
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
            let conn = conn.clone();
            tokio::spawn(async move {
                if let Err(e) = serve_stream(&conn, backend.as_ref(), send, recv).await {
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
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<()> {
    let BlobsMessage::Hello { certificate } = frame_io::expect(&mut recv).await? else {
        return reject(&mut send, BlobsErrorCode::ProtocolViolation).await;
    };
    let peer = match verify_peer(&certificate, conn) {
        Ok(peer) => peer,
        Err(_) => return reject(&mut send, BlobsErrorCode::NotAMember).await,
    };
    match frame_io::expect(&mut recv).await? {
        BlobsMessage::Request { folder, chunks } => {
            if chunks.len() > MAX_CHUNKS_PER_REQUEST {
                return reject(&mut send, BlobsErrorCode::TooManyChunks).await;
            }
            if !backend.is_member(&folder, &peer) {
                return reject(&mut send, BlobsErrorCode::NotAMember).await;
            }
            for hash in chunks {
                let reply = match backend.chunk(&folder, &hash) {
                    Some(data) => BlobsMessage::Chunk { hash, data },
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
        match frame_io::expect(&mut recv).await? {
            BlobsMessage::Chunk { hash, data } if hash == *expected => {
                if blake3::hash(&data).as_bytes() != hash.as_bytes() {
                    return Err(Error::Corrupt(hash));
                }
                results.push(ChunkResult::Found(hash, data));
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
    match frame_io::expect(&mut recv).await? {
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
    match frame_io::expect(&mut recv).await? {
        BlobsMessage::Replica { full } => Ok(full),
        BlobsMessage::Error { code } => Err(Error::Rejected(format!("{code:?}"))),
        _ => Err(Error::Protocol("unexpected replica response".into())),
    }
}
