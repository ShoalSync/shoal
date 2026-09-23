//! Tidefell `proto` crate: wire types, ALPN constants, signed message formats.

/// ALPN for the version 1 sync protocol. See `specs/sync-v1.md`.
pub const ALPN_SYNC_V1: &[u8] = b"tidefell/sync/1";

/// ALPN for the version 1 blob transfer protocol. See `specs/blobs-v1.md`.
pub const ALPN_BLOBS_V1: &[u8] = b"tidefell/blobs/1";
