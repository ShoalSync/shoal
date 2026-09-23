//! Tidefell `engine` crate: replica policies; wires chunker, store, meta, identity, net and
//! fetch into the daemon core.
//!
//! An [`Engine`] syncs a set of folders with peers. For each folder it
//!
//! - **scans** the folder on disk, chunks new and changed files into the block store and
//!   records them as local changes;
//! - **serves** its index, manifests and chunks to member peers over `net`;
//! - **materialises** remote changes: fetches missing chunks from peers, writes files
//!   atomically and applies deletes, never overwriting a file changed locally since the last
//!   scan (the next scan turns that edit into a conflict, which edits win);
//! - **purges** recycle-bin entries past their retention and garbage-collects their chunks.
//!
//! # On-disk layout
//!
//! ```text
//! <data>/blocks/                 block store shared by all folders
//! <data>/folders/<id>/meta       folder metadata snapshot (`meta` format)
//! <data>/folders/<id>/state      engine state: manifests, local file cache, peer progress
//! <folder root>/.tidefell/tmp/   in-progress downloads, renamed into place when complete
//! ```
//!
//! Phase 2 limitations: empty directories and symlinks are not synced, conflict copies are
//! kept in metadata but not written to disk, and partial replicas hold metadata only.

mod backend;
mod config;
mod engine;
mod materialize;
mod scan;
mod state;

pub use config::{EngineConfig, FolderConfig, PeerAddr, ReplicaMode};
pub use engine::{Engine, FolderStatus};
pub use tidefell_meta::Retention;
pub use tidefell_net::RelayConfig;

/// Engine errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem operation failed.
    #[error("I/O error on {path}: {source}")]
    Io {
        /// The path involved.
        path: std::path::PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The block store failed.
    #[error(transparent)]
    Store(#[from] tidefell_store::Error),
    /// Folder metadata failed to load, save or apply.
    #[error(transparent)]
    Meta(#[from] tidefell_meta::Error),
    /// The network layer failed.
    #[error(transparent)]
    Net(#[from] tidefell_net::Error),
    /// Engine state on disk is unreadable.
    #[error("corrupt engine state in {0}")]
    CorruptState(std::path::PathBuf),
    /// A folder named in a request is not configured.
    #[error("unknown folder {0}")]
    UnknownFolder(tidefell_proto::FolderId),
    /// Randomness was unavailable.
    #[error("no randomness available")]
    Random,
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_err(path: impl Into<std::path::PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}

pub(crate) fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).map_err(|_| Error::Random)?;
    Ok(out)
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
