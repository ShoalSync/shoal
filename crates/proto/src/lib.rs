//! Tidefell `proto` crate: wire types, ALPN constants, signed message formats.
//!
//! Everything in this crate is part of the wire contract described in `specs/`. Changing the
//! field order or types of any serialised struct or enum is a breaking change and requires a
//! new ALPN version. See ADR 0006.
// Peers' data is parsed here: an unchecked index, overflow or unwrap must be a reviewed
// exception (ADR 0031). Tests may panic.
#![cfg_attr(
    not(test),
    warn(
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::unwrap_used,
        clippy::expect_used
    )
)]

pub mod blobs;
pub mod blobs2;
mod bytes;
pub mod frame;
pub mod identity;
pub mod ids;
pub mod manifest;
pub mod pair;
pub mod signed;
pub mod sync;
pub mod sync2;
pub mod version;

pub use ids::{
    ChunkHash, ChunkId, ContentHash, DeviceId, EntryId, FolderId, KeyId, ListId, PublicKey, UserId,
};
pub use manifest::{ChunkRef, Manifest};
pub use signed::{Signed, SignedKind};
pub use version::{Causality, VersionVector, MAX_COUNTER, MAX_HOLES};

/// ALPN for the version 1 sync protocol. See `specs/sync-v1.md`.
pub const ALPN_SYNC_V1: &[u8] = b"tidefell/sync/1";

/// ALPN for the version 1 blob transfer protocol. See `specs/blobs-v1.md`.
pub const ALPN_BLOBS_V1: &[u8] = b"tidefell/blobs/1";

/// ALPN for the version 1 pairing protocol. See `specs/pair-v1.md`.
pub const ALPN_PAIR_V1: &[u8] = b"tidefell/pair/1";

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
    /// Text is not a valid pairing code.
    #[error("not a valid pairing code")]
    InvalidCode,
    /// A message decodes but breaks a rule of its spec.
    #[error("{0}")]
    Invalid(&'static str),
}
