//! Tidefell `meta` crate: file tree, versions, vector clocks, tombstones, recycle bin.
//!
//! A [`Folder`] is one replica's metadata for one shared folder: the current version of every
//! entry, unresolved conflict copies, the local index sequence used by `tidefell/sync/1`, and
//! the recycle bin. Conflict resolution follows `specs/sync-v1.md` so every replica converges
//! on the same state regardless of the order versions arrive in.

mod compact;
mod entries;
mod folder;
mod journal;
mod layout;
mod recycle;
mod resolve;
mod snapshot;
mod wire;

pub use compact::Current;
pub use folder::{
    copy_id, verify_remote, Applied, Change, ChangeKind, Folder, Indexed, StoredVersion, Verified,
};
pub use journal::{Header, Journal};
pub use layout::{Folding, Layout};
pub use recycle::{RecycleBin, Recycled, Retention};
pub use wire::{Seal, Wire};

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
    /// A version vector passes the counter ceiling (ADR 0024): a remote version holds a counter
    /// above it, or a local change would take this device's counter past it.
    #[error("version counter ceiling reached")]
    CounterCeiling,
    /// A local change would leave more holes in its vector than a version may have
    /// ([`tidefell_proto::MAX_HOLES`]): the entry has too many conflicts from this device that
    /// are not resolved.
    #[error("too many unresolved conflicts; resolve some first")]
    TooManyConflicts,
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
    /// A local change could not be sealed for an encrypted folder (ADR 0023).
    #[error("cannot seal the change: {0}")]
    Sealing(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
