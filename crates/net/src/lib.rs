//! Tidefell `net` crate: iroh endpoint, relay config, address lookup, sync/1 protocol.
//!
//! A [`Node`] owns the device's iroh endpoint and serves `tidefell/sync/1` and
//! `tidefell/blobs/1`, and can approve a pairing (`tidefell/pair/1`); a new device uses a
//! [`PairingServer`] instead until it has a certificate. It reaches folder metadata and chunks only through the [`Backend`] trait,
//! which `engine` implements, so this crate does not depend on `meta` or `store`.

mod backend;
mod blobs;
mod frame_io;
mod node;
mod pair;
mod sync;

pub use backend::{Backend, IndexPage};
pub use blobs::ChunkResult;
pub use iroh::{EndpointAddr, EndpointId, RelayUrl};
pub use node::{Node, NodeConfig, PeerHandle, RelayConfig};
pub use pair::{ApproverSession, Paired, PairingEvent, PairingServer};

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
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
