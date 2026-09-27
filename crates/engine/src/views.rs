//! Read-only views and events the engine exposes to front-ends (API, CLI, web).

use std::path::PathBuf;

use tidefell_meta::Retention;
use tidefell_proto::{DeviceId, EntryId, FolderId, UserId};

use crate::config::{ReplicaMode, SyncOptions};

/// A synced folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderInfo {
    /// Folder ID.
    pub id: FolderId,
    /// Where the folder lives on this device.
    pub path: PathBuf,
    /// Full or partial replica.
    pub mode: ReplicaMode,
    /// Users allowed to sync the folder.
    pub members: Vec<UserId>,
    /// Recycle-bin retention.
    pub retention: Retention,
    /// How the folder syncs on this device.
    pub sync: SyncOptions,
    /// What this replica's recycle bin holds.
    pub recycle_bin: RecycleSummary,
    /// True if the folder comes from the configuration file (and so cannot be changed or
    /// removed at runtime).
    pub configured: bool,
    /// Live files made on this device that no full replica has confirmed yet (ADR 0022).
    pub only_here: usize,
    /// Why the folder is scanned on its interval instead of watched for changes, if it is
    /// (ADR 0029).
    pub watch_error: Option<String>,
}

/// A summary of one replica's recycle bin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecycleSummary {
    /// Number of deleted entries, including files removed from this device only.
    pub items: usize,
    /// Total size of the deleted files, in bytes. For files removed from this device only, this
    /// is also the space emptying the bin frees here.
    pub bytes: u64,
    /// When the next item will be purged, or `None` if none will be.
    pub next_expiry_ms: Option<i64>,
}

/// A live file in a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// Stable entry ID.
    pub entry: EntryId,
    /// Path within the folder.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub modified_ms: i64,
    /// Whether the file is executable.
    pub executable: bool,
    /// Whether the current version is on this device's disk.
    pub on_disk: bool,
    /// Number of unresolved conflict copies.
    pub conflicts: usize,
    /// The device that made the current version.
    pub author: DeviceId,
    /// Full replicas that confirmed holding the current version's content (ADR 0022). Only
    /// tracked for content made on this device; 0 for everything else.
    pub confirmed_copies: usize,
    /// True if this device made the current version and no full replica has confirmed it:
    /// this device may hold the only copy.
    pub only_here: bool,
}

/// A full replica that confirmed holding a file's content (ADR 0022).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyInfo {
    /// The device.
    pub device: DeviceId,
    /// Its name when it confirmed.
    pub name: String,
    /// When it confirmed, milliseconds since the Unix epoch.
    pub confirmed_ms: i64,
}

/// A deleted file in the recycle bin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycledInfo {
    /// Entry ID, used to restore it.
    pub entry: EntryId,
    /// Path the file had.
    pub path: String,
    /// Size in bytes (0 for non-files).
    pub size: u64,
    /// When it was deleted, milliseconds since the Unix epoch.
    pub deleted_at_ms: i64,
    /// When it will be purged, or `None` to keep forever.
    pub expires_at_ms: Option<i64>,
    /// True if the file was removed from this device only (ADR 0025): other devices still
    /// have it, and restoring it puts it back here only. Otherwise it was deleted everywhere,
    /// and restoring it restores it everywhere.
    pub here_only: bool,
}

/// A connected peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// The peer device.
    pub device: DeviceId,
    /// The user it belongs to.
    pub user: UserId,
    /// Its name from its certificate.
    pub name: String,
    /// Folders shared with it.
    pub shared_folders: Vec<FolderId>,
}

/// Something that changed, for live displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A folder was added.
    FolderAdded(FolderId),
    /// A folder was removed.
    FolderRemoved(FolderId),
    /// A folder's index changed (local or remote edits, membership).
    FolderChanged(FolderId),
    /// A file was written to disk.
    FileUpdated {
        /// The folder.
        folder: FolderId,
        /// The file's path.
        path: String,
    },
    /// A file was removed from disk.
    FileRemoved {
        /// The folder.
        folder: FolderId,
        /// The file's path.
        path: String,
    },
}
