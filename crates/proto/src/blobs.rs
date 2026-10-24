//! Messages of the `shoal/blobs/1` protocol. See `specs/blobs-v1.md`.
//!
//! Variant order is part of the wire format and never changes. A new request may be appended
//! when older peers failing to decode it is handled (ADR 0022); anything else needs a new
//! protocol version.

use serde::{Deserialize, Serialize};

use crate::identity::DeviceCertificate;
use crate::{ChunkHash, ContentHash, FolderId, Signed};

/// Most chunks a single [`BlobsMessage::Request`] may ask for.
pub const MAX_CHUNKS_PER_REQUEST: usize = 64;

/// Why a peer rejected a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobsErrorCode {
    /// The requester is not a member of the folder.
    NotAMember,
    /// The folder is unknown.
    UnknownFolder,
    /// The request asked for more than [`MAX_CHUNKS_PER_REQUEST`] chunks.
    TooManyChunks,
    /// The message was malformed or unexpected.
    ProtocolViolation,
}

/// A message on a `shoal/blobs/1` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobsMessage {
    /// First message from the requester on each stream.
    Hello {
        /// The requesting device's certificate.
        certificate: Signed<DeviceCertificate>,
    },
    /// Ask for chunks of content in `folder`.
    Request {
        /// Folder the chunks belong to; used for access control.
        folder: FolderId,
        /// Chunks wanted, at most [`MAX_CHUNKS_PER_REQUEST`].
        chunks: Vec<ChunkHash>,
    },
    /// One requested chunk. The receiver verifies `data` hashes to `hash`.
    Chunk {
        /// The chunk.
        hash: ChunkHash,
        /// Its bytes.
        data: Vec<u8>,
    },
    /// The responder does not hold a requested chunk.
    Missing {
        /// The chunk.
        hash: ChunkHash,
    },
    /// Ask which chunks of a file's content the responder holds.
    AvailabilityRequest {
        /// Folder the content belongs to.
        folder: FolderId,
        /// The content.
        content: ContentHash,
    },
    /// Answer to [`BlobsMessage::AvailabilityRequest`].
    Availability {
        /// The content.
        content: ContentHash,
        /// Number of chunks in the content's manifest.
        chunk_count: u32,
        /// One bit per chunk in manifest order, least significant bit first; bit `i` of byte
        /// `i / 8` is chunk `i`.
        bitmap: Vec<u8>,
    },
    /// A request was rejected.
    Error {
        /// Reason.
        code: BlobsErrorCode,
    },
    /// Ask whether the responder is a full replica of `folder` (ADR 0022). Peers from before
    /// this message close the stream without a reply.
    ReplicaRequest {
        /// The folder.
        folder: FolderId,
    },
    /// Answer to [`BlobsMessage::ReplicaRequest`].
    Replica {
        /// True if the responder keeps every file of the folder, false for a partial replica.
        full: bool,
    },
    /// The responder is at its upload limit (ADR 0029). It sends no more chunks for this
    /// request; ask again for the rest after `retry_after_ms`, or ask another peer.
    Busy {
        /// Milliseconds to wait before asking this responder again.
        retry_after_ms: u32,
    },
}
