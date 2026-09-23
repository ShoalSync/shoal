//! `/v1` request and response bodies.
//!
//! Identifiers (devices, users, folders, entries) are lowercase hex strings. Times are
//! milliseconds since the Unix epoch.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// This device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Status {
    /// Device ID.
    pub device: String,
    /// The user this device belongs to.
    pub user: String,
    /// Device name from its certificate.
    pub name: String,
    /// Daemon version.
    pub version: String,
}

/// Full or partial replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Keeps every file.
    Full,
    /// Keeps metadata for everything and content within a budget.
    Partial,
}

/// A synced folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Folder {
    /// Folder ID.
    pub id: String,
    /// Where the folder lives on the device.
    pub path: String,
    /// Replica mode.
    pub mode: Mode,
    /// Block store budget for partial replicas.
    pub budget_bytes: Option<u64>,
    /// User IDs allowed to sync the folder.
    pub members: Vec<String>,
    /// Recycle-bin retention in days; `null` keeps deleted files forever.
    pub retention_days: Option<u32>,
    /// True if the folder comes from the configuration file and cannot be changed through
    /// the API.
    pub configured: bool,
}

/// Creates a folder, or joins one shared from another device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NewFolder {
    /// The folder ID to join. Omit to create a new folder with a fresh ID.
    #[serde(default)]
    pub id: Option<String>,
    /// Where the folder lives on this device. Created if missing.
    pub path: String,
    /// User IDs allowed to sync the folder. Defaults to this device's user.
    #[serde(default)]
    pub members: Vec<String>,
    /// Recycle-bin retention in days; 0 keeps deleted files forever.
    pub retention_days: u32,
    /// Replica mode; defaults to full.
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Block store budget; required for partial replicas.
    #[serde(default)]
    pub budget_bytes: Option<u64>,
    /// Seconds between scans for local changes; defaults to 10.
    #[serde(default)]
    pub scan_interval_secs: Option<u64>,
}

/// A user to add to a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NewMember {
    /// User ID.
    pub user: String,
}

/// A live file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct File {
    /// Stable entry ID.
    pub entry: String,
    /// Path within the folder, `/`-separated.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Modification time.
    pub modified_ms: i64,
    /// Whether the file is executable.
    pub executable: bool,
    /// Whether the current version is on this device's disk.
    pub on_disk: bool,
    /// Number of unresolved conflict copies.
    pub conflicts: usize,
    /// Device that made the current version.
    pub author: String,
}

/// A deleted file in the recycle bin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Recycled {
    /// Entry ID; restore with `POST /v1/folders/{id}/recycle/{entry}/restore`.
    pub entry: String,
    /// Path the file had.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// When it was deleted.
    pub deleted_at_ms: i64,
    /// When it will be purged; `null` if kept forever.
    pub expires_at_ms: Option<i64>,
}

/// A peer with an active sync session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Peer {
    /// Device ID.
    pub device: String,
    /// The user it belongs to.
    pub user: String,
    /// Device name.
    pub name: String,
    /// Folder IDs shared with it.
    pub shared_folders: Vec<String>,
}

/// A peer this device keeps connected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct KnownPeer {
    /// Device ID.
    pub device: String,
    /// Known direct addresses, e.g. "192.168.1.20:7777".
    #[serde(default)]
    pub addrs: Vec<String>,
}

/// A change, streamed from `GET /v1/events` as server-sent events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A folder was added.
    FolderAdded {
        /// Folder ID.
        folder: String,
    },
    /// A folder was removed.
    FolderRemoved {
        /// Folder ID.
        folder: String,
    },
    /// A folder's index or membership changed.
    FolderChanged {
        /// Folder ID.
        folder: String,
    },
    /// A file was written to disk.
    FileUpdated {
        /// Folder ID.
        folder: String,
        /// File path.
        path: String,
    },
    /// A file was removed from disk.
    FileRemoved {
        /// Folder ID.
        folder: String,
        /// File path.
        path: String,
    },
}

/// Why a request failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The request was malformed or invalid.
    BadRequest,
    /// The folder, entry or peer does not exist.
    NotFound,
    /// The resource already exists, or cannot be changed (e.g. configured folders).
    Conflict,
    /// Something failed on the server.
    Internal,
}

/// Error response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ApiError {
    /// Machine-readable category.
    pub kind: ErrorKind,
    /// Human-readable message.
    pub message: String,
}
