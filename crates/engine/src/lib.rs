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
//! - **purges** recycle-bin entries past their retention and garbage-collects their chunks;
//! - in a **partial replica**, keeps files as placeholders until they are opened, and evicts
//!   the least recently used ones to stay within the folder's budget (ADR 0016);
//! - **confirms copies**: asks connected full replicas whether they hold the files this device
//!   made, and records those that do (ADR 0022). A partial replica evicts a file it made only
//!   once it has a confirmed copy.
//!
//! # On-disk layout
//!
//! ```text
//! <data>/blocks/                 block store shared by all folders
//! <data>/folders/<id>/meta       folder metadata snapshot (`meta` format)
//! <data>/folders/<id>/state      engine state: manifests, local files, peer progress, copies
//! <folder root>/.tidefell/tmp/   in-progress downloads, renamed into place when complete
//! ```
//!
//! Folders and peers can also be added at runtime; they are remembered in `<data>/runtime`.
//!
//! Limitations: empty directories and symlinks are not synced, and conflict copies are kept in
//! metadata but not written to disk.

mod backend;
mod config;
mod copies;
pub mod device;
mod engine;
mod evict;
mod materialize;
pub mod pairing;
mod runtime;
mod scan;
mod state;
mod views;

pub use config::{EngineConfig, FolderConfig, PeerAddr, ReplicaMode};
pub use engine::{Engine, FolderStatus};
pub use tidefell_identity::{DeviceKey, UserKey};
pub use tidefell_meta::Retention;
pub use tidefell_net::{RelayConfig, RelayUrl};
pub use tidefell_proto::identity::DeviceCertificate;
pub use tidefell_proto::{DeviceId, EntryId, FolderId, Signed, UserId};
pub use views::{CopyInfo, Event, FileInfo, FolderInfo, PeerInfo, RecycleSummary, RecycledInfo};

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
    /// A folder with this ID already exists.
    #[error("folder {0} already exists")]
    FolderExists(tidefell_proto::FolderId),
    /// The folder comes from the configuration file and cannot be changed at runtime.
    #[error("folder {0} is managed by the configuration file")]
    Configured(tidefell_proto::FolderId),
    /// The entry is not in the folder's recycle bin.
    #[error("entry {0} is not in the recycle bin")]
    NotRecycled(tidefell_proto::EntryId),
    /// No live file has this path.
    #[error("no such file: {0}")]
    NoSuchFile(String),
    /// A file's content could not be fetched from connected peers.
    #[error("{0} is not available: no connected peer has its content")]
    Unavailable(String),
    /// A request was invalid.
    #[error("{0}")]
    Invalid(String),
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
