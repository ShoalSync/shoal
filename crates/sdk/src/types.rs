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

/// Whether other devices' changes are written to this device.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// Sends local changes and writes other devices' changes to disk.
    #[default]
    Both,
    /// Sends local changes only; other devices' changes are never written here.
    SendOnly,
}

/// What deleting a file on this device, outside Tidefell, does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LocalDeletes {
    /// The delete is synced: the file moves to every device's recycle bin.
    #[default]
    Everywhere,
    /// The file is dropped from this device only; it stays on the others and is not downloaded
    /// again until fetched.
    ThisDevice,
}

/// How a folder syncs on this device. Applies to this device only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SyncOptions {
    /// Whether other devices' changes are written here.
    #[serde(default)]
    pub direction: Direction,
    /// What deleting a file here does.
    #[serde(default)]
    pub local_deletes: LocalDeletes,
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
    /// How the folder syncs on this device.
    #[serde(default)]
    pub sync: SyncOptions,
    /// What this device's recycle bin holds for the folder.
    #[serde(default)]
    pub recycle_bin: RecycleBinSummary,
    /// True if the folder comes from the configuration file and cannot be changed through
    /// the API.
    pub configured: bool,
    /// Live files made on this device that no full replica has confirmed holding yet: this
    /// device may hold their only copy.
    #[serde(default)]
    pub only_here: u64,
}

/// A summary of one device's recycle bin for a folder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RecycleBinSummary {
    /// Number of deleted files.
    pub items: u64,
    /// Total size of the deleted files in bytes.
    pub bytes: u64,
    /// When the next item will be purged; `null` if none will be.
    pub next_expiry_ms: Option<i64>,
}

/// A new recycle-bin retention for a folder. Applies to this device only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RetentionUpdate {
    /// Days to keep deleted files; `null` or 0 keeps them forever. Items past a shortened
    /// retention are purged at the next hourly purge.
    pub retention_days: Option<u32>,
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
    /// Bytes of files a partial replica keeps on disk; required for partial replicas.
    #[serde(default)]
    pub budget_bytes: Option<u64>,
    /// Seconds between scans for local changes; defaults to 10.
    #[serde(default)]
    pub scan_interval_secs: Option<u64>,
    /// How the folder syncs on this device; defaults to both directions, with deletes synced.
    #[serde(default)]
    pub sync: SyncOptions,
}

/// A file to remove from this device only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RemoveHereRequest {
    /// Path within the folder.
    pub path: String,
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
    /// Whether the current version is on this device's disk. In a partial replica, files not
    /// on disk are placeholders, fetched with `POST /v1/folders/{id}/fetch`.
    pub on_disk: bool,
    /// Number of unresolved conflict copies.
    pub conflicts: usize,
    /// Device that made the current version.
    pub author: String,
    /// Full replicas that confirmed holding this file. Only tracked for files made on this
    /// device; 0 otherwise.
    #[serde(default)]
    pub confirmed_copies: u64,
    /// True if this device made the current version and no full replica has confirmed
    /// holding it yet.
    #[serde(default)]
    pub only_here: bool,
}

/// A file whose confirmed copies are wanted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CopiesRequest {
    /// Path within the folder.
    pub path: String,
}

/// A full replica that confirmed holding a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Copy {
    /// Its device ID.
    pub device: String,
    /// Its name when it confirmed.
    pub name: String,
    /// When it confirmed.
    pub confirmed_ms: i64,
}

/// A file to make available on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FetchRequest {
    /// Path within the folder.
    pub path: String,
}

/// Where a fetched file is on the daemon's disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Fetched {
    /// Absolute path on the daemon's machine.
    pub local_path: String,
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
    /// True if the file was removed from this device only: other devices still have it, and
    /// restoring it puts it back here only.
    #[serde(default)]
    pub here_only: bool,
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
    /// A relay the peer is reachable through.
    #[serde(default)]
    pub relay: Option<String>,
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

/// Start approving a new device from its pairing code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PairRequest {
    /// The pairing code the new device shows (`TFP1-…`).
    pub code: String,
}

/// A pairing waiting for the person to compare confirmation codes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Pairing {
    /// Identifies this pairing in later requests.
    pub id: String,
    /// The new device's ID.
    pub device: String,
    /// The name the new device suggests for itself.
    pub suggested_name: String,
    /// Six digits the new device also shows.
    pub confirmation_code: String,
}

/// Approve a pairing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ApprovePairing {
    /// The new device's name; defaults to the name it suggested.
    #[serde(default)]
    pub name: Option<String>,
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
    /// The content is not available from any connected peer right now.
    Unavailable,
}

/// Error response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ApiError {
    /// Machine-readable category.
    pub kind: ErrorKind,
    /// Human-readable message.
    pub message: String,
}
