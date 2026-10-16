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
    /// Whether this device announces itself and finds known devices on the local network.
    pub local_discovery: bool,
    /// The most recent crash the user has not been told about yet (ADR 0030).
    #[serde(default)]
    pub crash: Option<Crash>,
    /// Syncing is paused for the whole device (ADR 0029).
    #[serde(default)]
    pub paused: bool,
    /// Every device this one syncs with, connected or not (ADR 0030).
    #[serde(default)]
    pub peers: Vec<PeerState>,
    /// File content sent and received since the daemon started.
    #[serde(default)]
    pub transfer: Transfer,
    /// Where each folder's metadata database is kept (ADR 0034).
    #[serde(default)]
    pub metadata_dir: String,
    /// This device's user has revoked it (ADR 0040): it syncs with no one; pair it again.
    #[serde(default)]
    pub revoked: bool,
}

/// How a connection reaches its peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    /// Directly over IP.
    Direct,
    /// Through a relay server.
    Relay,
}

/// A device this one syncs with, connected or not (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PeerState {
    /// Device ID.
    pub device: String,
    /// Its name, if it has connected since the daemon started.
    #[serde(default)]
    pub name: Option<String>,
    /// A sync session is running.
    pub connected: bool,
    /// How the connection goes, while connected.
    #[serde(default)]
    pub path: Option<PathKind>,
    /// Round-trip time in milliseconds, while connected.
    #[serde(default)]
    pub rtt_ms: Option<u64>,
    /// When the current session started.
    #[serde(default)]
    pub connected_since_ms: Option<i64>,
    /// When a session was last up: now while connected.
    #[serde(default)]
    pub last_seen_ms: Option<i64>,
    /// Why the last attempt to connect, or the last session, failed.
    #[serde(default)]
    pub last_error: Option<String>,
    /// How far its clock was ahead of this device's at the last handshake, in milliseconds.
    #[serde(default)]
    pub clock_offset_ms: Option<i64>,
}

/// Asks for a diagnostics bundle (ADR 0030).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct DiagnosticsRequest {
    /// Keep real paths and folder names instead of hashes. IDs are shortened and addresses
    /// reduced to their kind either way.
    #[serde(default)]
    pub include_paths: bool,
}

/// What a change in the activity log did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    /// A new file.
    Added,
    /// New content.
    Changed,
    /// Deleted.
    Deleted,
    /// Moved to another path.
    Renamed,
    /// Live again after it was deleted.
    Restored,
    /// A version made apart from the current one arrived, and waits for a decision.
    Conflict,
}

/// How many changes `GET /v1/folders/{id}/activity` returns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::IntoParams)]
pub struct ActivityQuery {
    /// Most changes to return; 100 if left out.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One change the daemon applied to a folder (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Activity {
    /// When the daemon applied it.
    pub time_ms: i64,
    /// What happened.
    pub kind: ChangeKind,
    /// The file's path.
    pub path: String,
    /// Its path before, for a rename.
    #[serde(default)]
    pub old_path: Option<String>,
    /// The device that made the change.
    pub device: String,
    /// That device's name, as far as the daemon knows it, or the start of its ID.
    pub device_name: String,
    /// Made on this device, rather than received from a peer.
    pub local: bool,
}

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum CheckLevel {
    /// Fine.
    Pass,
    /// Works, but worth attention.
    Warning,
    /// Something does not work.
    Failure,
}

/// One check of `GET /v1/doctor` (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Check {
    /// What was checked: `state`, `clock`, `space`, `folder`, `filesystem`, `watch`, `relay`,
    /// `network` or `peers`.
    pub name: String,
    /// How it came out.
    pub level: CheckLevel,
    /// What was found.
    pub detail: String,
    /// What to do about it, for a warning or failure.
    #[serde(default)]
    pub advice: Option<String>,
}

/// File content moved since the daemon started. Index exchanges are not counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Transfer {
    /// Bytes sent.
    pub sent: u64,
    /// Bytes received.
    pub received: u64,
    /// Bytes per second sent, over the last ten seconds.
    pub send_rate: u64,
    /// Bytes per second received, over the last ten seconds.
    pub receive_rate: u64,
}

/// What a folder is doing (ADR 0030).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum FolderState {
    /// Nothing to do.
    #[default]
    Idle,
    /// Looking for local changes.
    Scanning,
    /// Files are pending and a connected peer shares the folder.
    Syncing,
    /// Paused.
    Paused,
    /// Its metadata is being started over, and it waits for a peer's index; see `rebuild`.
    Rebuilding,
    /// The folder cannot be used, or its last scan failed; see `error`.
    Error,
}

/// A folder's rebuild (ADR 0034): its metadata was started over, from the files on disk and
/// what peers send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Rebuild {
    /// When it began, in milliseconds since the Unix epoch.
    pub started_ms: i64,
    /// Why, in words.
    pub reason: String,
    /// When it finished, if it has: a peer's whole index arrived, or there was no peer.
    #[serde(default)]
    pub finished_ms: Option<i64>,
    /// While it has not: the connected devices sharing the folder whose index it waits for.
    #[serde(default)]
    pub waiting_for: Vec<String>,
}

/// A folder's exchanges with one peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FolderPeer {
    /// Device ID.
    pub device: String,
    /// When the folder last finished exchanging changes with it, since the daemon started.
    #[serde(default)]
    pub last_exchange_ms: Option<i64>,
    /// Connected and caught up; later changes are pushed as they happen.
    pub up_to_date: bool,
}

/// What a failing path was being used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    /// Reading it for changes.
    Scan,
    /// Fetching its content from other devices.
    Fetch,
    /// Writing it to disk.
    Write,
    /// Deleting it from disk.
    Delete,
}

/// Why a path failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// Not allowed, including a read-only disk.
    PermissionDenied,
    /// No space left.
    DiskFull,
    /// No connected device has the content.
    ContentUnavailable,
    /// A name this device cannot sync.
    NameNotAllowed,
    /// The file changed while it was read or written; tried again later.
    ChangedWhileWriting,
    /// Anything else; see the message.
    Other,
}

/// A path that failed, until it succeeds (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PathProblem {
    /// Path within the folder.
    pub path: String,
    /// What it was being used for.
    pub operation: Operation,
    /// Why it failed.
    pub kind: ProblemKind,
    /// The error.
    pub message: String,
    /// When it first failed.
    pub since_ms: i64,
    /// How many passes in a row it has failed.
    pub attempts: u32,
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
    /// Why the folder is scanned on an interval instead of watched for changes, if it is.
    #[serde(default)]
    pub watch_error: Option<String>,
    /// Why the folder's root cannot be used, if it cannot, as when its drive is not mounted.
    /// Nothing is scanned, written or deleted there until it is back.
    #[serde(default)]
    pub unavailable: Option<String>,
    /// Syncing of this folder is paused; see also [`Status::paused`].
    #[serde(default)]
    pub paused: bool,
    /// Total size of the files pinned on this partial replica; not counted against the budget.
    #[serde(default)]
    pub pinned_bytes: u64,
    /// Files with versions that conflict with the current one, waiting for a decision.
    #[serde(default)]
    pub conflicts: u64,
    /// What the folder is doing (ADR 0030).
    #[serde(default)]
    pub state: FolderState,
    /// Why the folder is in the error state.
    #[serde(default)]
    pub error: Option<String>,
    /// Files this device is to write and does not have yet.
    #[serde(default)]
    pub pending_files: u64,
    /// Their total size.
    #[serde(default)]
    pub pending_bytes: u64,
    /// When the last scan finished.
    #[serde(default)]
    pub last_scan_ms: Option<i64>,
    /// How long the last scan took, in milliseconds.
    #[serde(default)]
    pub last_scan_duration_ms: Option<u64>,
    /// Peers the folder is shared with now, or was exchanged with since the daemon started.
    #[serde(default)]
    pub peers: Vec<FolderPeer>,
    /// Paths that failed to scan, fetch, write or delete; `GET /v1/folders/{id}/problems`.
    #[serde(default)]
    pub problems: u64,
    /// The folder's last rebuild, if its metadata was ever started over (ADR 0034).
    #[serde(default)]
    pub rebuild: Option<Rebuild>,
    /// Files the last scan found deleted, so many that the deletes wait for a decision:
    /// `GET` and `POST /v1/folders/{id}/deletes` (ADR 0037).
    #[serde(default)]
    pub held_deletes: u64,
    /// Why nothing more is downloaded to this folder, if a drive is below its free-space floor
    /// (ADR 0037).
    #[serde(default)]
    pub low_space: Option<String>,
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

/// What to do with the deletes a scan held because there were so many (ADR 0037).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HeldDeletes {
    /// Delete the files on every device, each keeping them in its recycle bin.
    Confirm,
    /// Fetch the files back from other devices; nothing is deleted.
    Restore,
}

/// A decision on a folder's held deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct HeldDeletesDecision {
    /// Confirm or restore.
    pub action: HeldDeletes,
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

/// A file or directory to pin or unpin on a partial replica (ADR 0029).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PinRequest {
    /// Path within the folder: a file, or a directory and everything below it.
    pub path: String,
}

/// A file or directory to rename or move on every device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RenameRequest {
    /// Its path within the folder.
    pub from: String,
    /// Its new path within the folder.
    pub to: String,
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
    /// That device's name, as far as the daemon knows it, or the start of its ID.
    #[serde(default)]
    pub author_name: String,
    /// Full replicas that confirmed holding this file. Only tracked for files made on this
    /// device; 0 otherwise.
    #[serde(default)]
    pub confirmed_copies: u64,
    /// True if this device made the current version and no full replica has confirmed
    /// holding it yet.
    #[serde(default)]
    pub only_here: bool,
    /// Pinned on this device (ADR 0029): kept on disk and never evicted.
    #[serde(default)]
    pub pinned: bool,
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
    /// True if this is a version that lost when a conflict was resolved (ADR 0029). Restoring
    /// it makes it a file of its own beside `path`.
    #[serde(default)]
    pub conflict: bool,
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

/// A device its user revoked (ADR 0040).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Revocation {
    /// The revoked device's ID.
    pub device: String,
    /// The user who revoked it.
    pub user: String,
    /// The device's name, if this device holds its certificate.
    #[serde(default)]
    pub name: Option<String>,
    /// When it was revoked, in milliseconds since the Unix epoch.
    pub revoked_ms: i64,
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
    /// A file gained a version that conflicts with its current one (ADR 0029).
    ConflictDetected {
        /// Folder ID.
        folder: String,
        /// File path.
        path: String,
    },
    /// A task panicked and the engine stopped; the daemon exits so it is restarted.
    Crashed {
        /// What went wrong.
        message: String,
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

/// How this device logs (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LogSettings {
    /// The level filter, such as `info` or `info,tidefell_net=debug`.
    pub level: String,
    /// The file logs are written to; none when the system journal keeps them.
    #[serde(default)]
    pub file: Option<String>,
}

/// A new level filter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LogLevel {
    /// The level filter, such as `debug` or `info,tidefell_engine=debug`.
    pub level: String,
}

/// A recent warning or error (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Problem {
    /// When, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// `WARN` or `ERROR`.
    pub level: String,
    /// Where it was logged.
    pub target: String,
    /// The message.
    pub message: String,
    /// Its structured fields, such as the folder and path.
    #[serde(default)]
    pub fields: std::collections::BTreeMap<String, String>,
}

/// A crash recorded on this device (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Crash {
    /// Names the crash for `GET /v1/crashes/report`: its file's name.
    #[serde(default)]
    pub id: String,
    /// The crash file on the daemon's machine.
    pub file: String,
    /// When, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// What went wrong.
    pub message: String,
    /// Whether the user has already been told.
    pub seen: bool,
}

/// Which crash `GET /v1/crashes/report` writes up.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::IntoParams)]
pub struct CrashReportQuery {
    /// The crash's `id`.
    pub id: String,
}

/// A crash written up as a new issue for the project's tracker (ADR 0030). Nothing is sent:
/// the user reviews it and files it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CrashReport {
    /// The issue's title.
    pub title: String,
    /// Its body, in Markdown, with home directories replaced by `~`.
    pub body: String,
    /// A link that opens a new issue pre-filled with them, if a tracker is configured; its body
    /// may be cut to fit a link. Without one, copy the title and body into a new issue.
    #[serde(default)]
    pub url: Option<String>,
}

/// A file whose versions are wanted, or one of them to restore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct VersionRequest {
    /// Path within the folder.
    pub path: String,
    /// The version to restore; unused when listing.
    #[serde(default)]
    pub version: Option<String>,
}

/// Decides a conflict on a file (ADR 0029).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ResolveRequest {
    /// Path within the folder.
    pub path: String,
    /// The version to keep, `current` or `conflict:<id>`, or `both` to keep the current version
    /// as the file and each other version as a file of its own beside it.
    pub keep: String,
}

/// A read-only copy of a version, on the daemon's machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OpenedVersion {
    /// The copy's path.
    pub file: String,
}

/// Which kind of version a [`Version`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum VersionKind {
    /// The version the folder has now.
    Current,
    /// A version kept on this device after it was replaced (ADR 0029).
    Previous,
    /// A version made apart from the current one, waiting for a decision (ADR 0029).
    Conflict,
}

/// One version of a file (ADR 0029).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Version {
    /// Identifies the version: `current`, `previous:<id>` or `conflict:<id>`.
    pub id: String,
    /// Which kind it is.
    pub kind: VersionKind,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, in milliseconds since the Unix epoch.
    pub modified_ms: i64,
    /// For the current version and conflicts, the device that made it.
    #[serde(default)]
    pub author: Option<String>,
    /// That device's name, as far as the daemon knows it.
    #[serde(default)]
    pub author_name: Option<String>,
    /// For a previous version, when this device kept it.
    #[serde(default)]
    pub kept_ms: Option<i64>,
    /// For a previous version, when it expires; none if the folder keeps deleted files forever.
    #[serde(default)]
    pub expires_ms: Option<i64>,
}

/// Upload and download limits for file content, in KiB per second (ADR 0029). Changes through the
/// API last until the daemon restarts; the configuration sets them for good.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Limits {
    /// Most KiB per second sent to other devices; none for no limit. At least 64.
    #[serde(default)]
    pub upload_kib_per_sec: Option<u64>,
    /// Most KiB per second received from other devices; none for no limit. At least 64.
    #[serde(default)]
    pub download_kib_per_sec: Option<u64>,
    /// Also limit traffic to devices on the local network.
    #[serde(default)]
    pub limit_local: bool,
}
