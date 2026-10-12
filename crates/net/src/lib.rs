//! Tidefell `net` crate: iroh endpoint, relay config, address lookup, sync/1 protocol.
//!
//! A [`Node`] owns the device's iroh endpoint and serves `tidefell/sync/1` and
//! `tidefell/blobs/1`, and can approve a pairing (`tidefell/pair/1`); a new device uses a
//! [`PairingServer`] instead until it has a certificate. It reaches folder metadata and chunks only through the [`Backend`] trait,
//! which `engine` implements, so this crate does not depend on `meta` or `store`.
// Peers' data is handled here: an unchecked index, overflow or unwrap must be a reviewed
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

mod backend;
mod blobs;
mod frame_io;
mod limits;
mod locks;
#[cfg(feature = "simulation")]
mod memory;
mod node;
mod pair;
mod sync;
mod traffic;
mod transport;

pub use backend::{Backend, IndexPage};
pub use blobs::ChunkResult;
pub use iroh::{EndpointAddr, EndpointId, RelayUrl};
pub use limits::{Limits, MIN_BYTES_PER_SEC};
#[cfg(feature = "simulation")]
pub use memory::Network;
pub use node::{
    Connectivity, NetworkReport, Node, NodeConfig, PathKind, PeerHandle, PeerStatus, RelayConfig,
    RelayState,
};
pub use pair::{ApproverSession, Paired, PairingEvent, PairingServer};
pub use traffic::{TrafficCounts, Transfer};

/// Network and protocol errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The endpoint could not be created.
    #[error("failed to start endpoint: {0}")]
    Bind(String),
    /// Connecting to a peer failed.
    #[error("failed to connect: {0}")]
    Connect(String),
    /// A QUIC stream failed.
    #[error("stream error: {0}")]
    Stream(String),
    /// A frame could not be decoded, or a message was unexpected.
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// The peer's device certificate is invalid or does not match the connection.
    #[error("peer authentication failed: {0}")]
    Auth(&'static str),
    /// The peer rejected the request.
    #[error("peer rejected the request: {0}")]
    Rejected(String),
    /// The peer sent data that does not match its hash.
    #[error("peer sent corrupt data for chunk {0}")]
    Corrupt(tidefell_proto::ChunkHash),
    /// The peer session has ended.
    #[error("peer session closed")]
    Closed,
    /// The peer did not respond in time (ADR 0024).
    #[error("peer did not respond within {0:?}")]
    Timeout(std::time::Duration),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// How long a peer has to complete a handshake (ADR 0024).
pub(crate) const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long a request may wait for a response, or a transfer for its next message (ADR 0024).
pub(crate) const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The largest message read from a peer before it has authenticated (ADR 0024).
pub(crate) const MAX_UNAUTHENTICATED: u64 = 64 * 1024;

/// Runs `future`, failing with [`Error::Timeout`] if it takes longer than `limit`.
pub(crate) async fn within<T>(
    limit: std::time::Duration,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(limit, future)
        .await
        .map_err(|_| Error::Timeout(limit))?
}

/// Milliseconds since the Unix epoch.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}
