//! Messages of the `tidefell/storage/1` protocol: how members rank what a storage peer keeps of
//! an encrypted folder. See `specs/storage-v1.md`.
//!
//! Variant order is part of the wire format, as in [`crate::blobs`].

use serde::{Deserialize, Serialize};

use crate::identity::DeviceCertificate;
use crate::{EntryId, FolderId, Signed};

/// Most hints one [`StorageMessage::Priorities`] may carry.
pub const MAX_HINTS: usize = 4096;

/// The priority of an entry no member has ranked.
pub const DEFAULT_PRIORITY: u8 = 128;

/// One entry's priority: higher is kept first, and 0 not at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hint {
    pub entry: EntryId,
    pub priority: u8,
}

/// Why a storage peer rejected a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageErrorCode {
    NotAMember,
    /// This device does not store the folder.
    NotStored,
    /// A request carried more hints than allowed.
    TooManyHints,
    ProtocolViolation,
}

/// A message on a `tidefell/storage/1` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StorageMessage {
    Hello {
        certificate: Signed<DeviceCertificate>,
    },
    /// Priorities of the folder's current entries, given at `issued_ms` by the sender's clock.
    Priorities {
        folder: FolderId,
        issued_ms: i64,
        hints: Vec<Hint>,
    },
    Accepted,
    Error {
        code: StorageErrorCode,
    },
    UsageRequest {
        folder: FolderId,
    },
    /// The folder's quota, the sealed bytes held of it, and the room left for it, under its
    /// quota and the storage peer's total.
    Usage {
        quota_bytes: u64,
        held_bytes: u64,
        room_bytes: u64,
    },
    /// Forget what is held of the entries ranked below `below`, as a user asked.
    Drop {
        folder: FolderId,
        below: u8,
    },
    Dropped {
        chunks: u64,
        bytes: u64,
    },
}
