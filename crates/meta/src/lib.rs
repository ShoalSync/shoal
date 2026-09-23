//! Tidefell `meta` crate: file tree, versions, vector clocks, tombstones, recycle bin.
//!
//! A [`Folder`] is one replica's metadata for one shared folder: the current version of every
//! entry, unresolved conflict copies, the local index sequence used by `tidefell/sync/1`, and
//! the recycle bin. Conflict resolution follows `specs/sync-v1.md` so every replica converges
//! on the same state regardless of the order versions arrive in.

mod folder;
mod recycle;
mod resolve;
mod snapshot;

pub use folder::{Applied, Folder, StoredVersion};
pub use recycle::{RecycleBin, Recycled, Retention};

/// Metadata errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A remote version failed verification.
    #[error(transparent)]
    Proto(#[from] tidefell_proto::Error),
    /// A version belongs to a different folder.
    #[error("version belongs to another folder")]
    WrongFolder,
    /// A path breaks the rules in `specs/sync-v1.md`.
    #[error("invalid path {0:?}")]
    InvalidPath(String),
    /// A local change was signed with a key that is not this replica's device.
    #[error("signing key does not belong to this device")]
    WrongDevice,
    /// The entry is not in the recycle bin, or has been recreated since.
    #[error("entry is not restorable")]
    NotRestorable,
    /// Reading or writing a snapshot failed.
    #[error("snapshot I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A snapshot could not be decoded.
    #[error("malformed snapshot: {0}")]
    Snapshot(postcard::Error),
    /// A snapshot is in a format this version cannot read.
    #[error("unsupported snapshot format")]
    UnsupportedFormat,
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
