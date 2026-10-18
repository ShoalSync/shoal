//! Messages of the `tidefell/blobs/2` protocol, for encrypted folders' sealed chunks and list
//! blobs. See `specs/blobs-v2.md`.
//!
//! Variant order is part of the wire format, as in [`crate::blobs`].

use serde::{Deserialize, Serialize};

use crate::identity::DeviceCertificate;
use crate::{FolderId, Signed};

/// Most blobs one [`BlobsMessage::Request`] may ask for.
pub const MAX_BLOBS_PER_REQUEST: usize = 64;
/// Most blobs one [`BlobsMessage::AvailabilityRequest`] may ask about.
pub const MAX_BLOBS_PER_AVAILABILITY: usize = 4096;
/// The largest sealed chunk: 1 MiB of plaintext, 4 KiB of padding and the tag.
pub const MAX_SEALED_CHUNK: usize = (1 << 20) + 4096 + 16;
/// The largest list blob: 4096 chunk IDs and their length prefix.
pub const MAX_LIST_BLOB: usize = 2 + 4096 * 32;

pub use crate::ids::BlobId;

/// Why a peer rejected a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobsErrorCode {
    NotAMember,
    UnknownFolder,
    /// A request named more blobs than allowed.
    TooManyBlobs,
    ProtocolViolation,
}

/// A message on a `tidefell/blobs/2` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlobsMessage {
    Hello {
        certificate: Signed<DeviceCertificate>,
    },
    Request {
        folder: FolderId,
        blobs: Vec<BlobId>,
    },
    Blob {
        id: BlobId,
        data: Vec<u8>,
    },
    Missing {
        id: BlobId,
    },
    AvailabilityRequest {
        folder: FolderId,
        blobs: Vec<BlobId>,
    },
    /// One bit per blob asked about, least significant bit first.
    Availability {
        bitmap: Vec<u8>,
    },
    Error {
        code: BlobsErrorCode,
    },
    ReplicaRequest {
        folder: FolderId,
    },
    Replica {
        full: bool,
    },
    Busy {
        retry_after_ms: u32,
    },
}
