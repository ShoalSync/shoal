//! Tidefell `proto` crate: wire types, ALPN constants, signed message formats.
//!
//! Everything in this crate is part of the wire contract described in `specs/`. Changing the
//! field order or types of any serialised struct or enum is a breaking change and requires a
//! new ALPN version. See ADR 0006.

pub mod blobs;
mod bytes;
pub mod frame;
pub mod identity;
pub mod ids;
pub mod manifest;
pub mod signed;
pub mod sync;
pub mod version;

pub use ids::{ChunkHash, ContentHash, DeviceId, EntryId, FolderId, PublicKey, UserId};
pub use manifest::{ChunkRef, Manifest};
pub use signed::{Signed, SignedKind};
pub use version::{Causality, VersionVector};

/// ALPN for the version 1 sync protocol. See `specs/sync-v1.md`.
pub const ALPN_SYNC_V1: &[u8] = b"tidefell/sync/1";

/// ALPN for the version 1 blob transfer protocol. See `specs/blobs-v1.md`.
pub const ALPN_BLOBS_V1: &[u8] = b"tidefell/blobs/1";

/// Errors decoding or verifying wire data.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The bytes are not a valid postcard encoding of the expected type.
    #[error("malformed message: {0}")]
    Decode(#[from] postcard::Error),
    /// A frame's declared length exceeds [`frame::MAX_FRAME_LEN`].
    #[error("frame of {0} bytes exceeds the maximum")]
    FrameTooLarge(u64),
    /// Input ended in the middle of a frame.
    #[error("truncated frame")]
    Truncated,
    /// The signer field is not a valid Ed25519 public key.
    #[error("invalid signer key")]
    InvalidKey,
    /// The signature does not verify against the signer and body.
    #[error("bad signature")]
    BadSignature,
    /// A signed message was signed by a different key than the one it claims.
    #[error("signer does not match the claimed author or user")]
    SignerMismatch,
    /// A hex string could not be parsed as an identifier.
    #[error("invalid hex identifier")]
    InvalidHex,
}
