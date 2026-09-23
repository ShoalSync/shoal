//! Messages of the `tidefell/blobs/1` protocol. See `specs/blobs-v1.md`.
//!
//! Variant order is part of the wire format. Append new variants only in a new protocol version.

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

/// A message on a `tidefell/blobs/1` stream.
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
}
