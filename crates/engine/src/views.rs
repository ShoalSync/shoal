//! Read-only views and events the engine exposes to front-ends (API, CLI, web).

use std::path::PathBuf;

use shoal_meta::Retention;
use shoal_proto::{DeviceId, EntryId, FolderId, UserId};

use shoal_net::PathKind;

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
    /// Why the folder's root cannot be used, if it cannot, as when its drive is not mounted:
    /// nothing is scanned, written or deleted there until it is back (ADR 0037).
    pub unavailable: Option<String>,
    /// Syncing of this folder is paused (ADR 0029). The whole device may be paused as well; see
    /// [`crate::Engine::paused`].
    pub paused: bool,
    /// Total size of the pinned files on a partial replica (ADR 0029), on disk or still to be
    /// fetched; they do not count against the budget.
    pub pinned_bytes: u64,
    /// Files with versions that conflict with the current one, waiting for a decision (ADR 0029).
    pub conflicts: usize,
    /// What the folder is doing (ADR 0030).
    pub state: FolderState,
    /// Why the folder is in [`FolderState::Error`].
    pub error: Option<String>,
    /// Files this device is to write and does not have yet.
    pub pending_files: usize,
    /// Their total size.
    pub pending_bytes: u64,
    /// When the last scan finished, in milliseconds since the Unix epoch.
    pub last_scan_ms: Option<i64>,
    /// How long the last scan took.
    pub last_scan_duration_ms: Option<u64>,
    /// The peers the folder is shared with now or was exchanged with since the engine started.
    pub peers: Vec<FolderPeer>,
    /// Paths that failed to scan, fetch, write or delete; see [`crate::Engine::path_problems`].
    pub problems: usize,
    /// Files the last scan found deleted, so many that the deletes wait for
    /// [`crate::Engine::confirm_deletes`] or [`crate::Engine::restore_deleted`] (ADR 0037).
    pub held_deletes: usize,
    /// Why nothing more is downloaded to this folder, if its drive or the engine's data drive
    /// is below its free-space floor (ADR 0037), with how much its recycle bin and earlier
    /// versions hold there.
    pub low_space: Option<String>,
    /// The folder's last rebuild, if its metadata was ever started over (ADR 0034).
    pub rebuild: Option<RebuildInfo>,
    /// For an encrypted folder, how (ADR 0023).
    pub encryption: Option<EncryptionInfo>,
}

/// How a folder is encrypted on this device (ADR 0023).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptionInfo {
    /// The devices this device syncs the folder with that hold it without reading it.
    pub storage_peers: Vec<DeviceId>,
    /// The generation of the folder key new content is sealed with, or `None` until a key has
    /// reached this device.
    pub key_generation: Option<u64>,
    /// Sealed versions kept unopened, waiting for a key that has not reached this device.
    pub waiting_versions: usize,
    /// On a storage peer, what it stores of the folder.
    pub stored: Option<StoredInfo>,
}

/// What a storage peer stores of a folder it cannot read (ADR 0023).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredInfo {
    /// Sealed versions it serves: those no other kept version supersedes.
    pub versions: usize,
    /// Superseded versions it keeps for the folder's retention.
    pub superseded: usize,
    /// The most it keeps of the folder's chunks, in bytes.
    pub quota_bytes: u64,
    /// The sealed bytes of the chunks it holds.
    pub bytes: u64,
    /// Chunks of its current versions it does not hold yet, but for entries members keep from
    /// it (priority 0).
    pub missing_chunks: usize,
    /// Entries members have given a priority (ADR 0023's amendment of 2026-10-22).
    pub ranked: usize,
}

/// What a storage peer says of a folder it stores for its members (ADR 0023's amendment of
/// 2026-10-22).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageUsage {
    /// Its quota for the folder.
    pub quota_bytes: u64,
    /// The sealed bytes it holds of it.
    pub held_bytes: u64,
    /// The room it has left for it, under the folder's quota and its total.
    pub room_bytes: u64,
}

/// The files of one priority, and what a storage peer holds of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageLevel {
    /// 0 to 255.
    pub priority: u8,
    /// Current files at it.
    pub files: usize,
    /// Their bytes, by their chunks' lengths.
    pub bytes: u64,
    /// The bytes of those chunks the storage peer holds.
    pub held_bytes: u64,
}

/// One storage peer of an encrypted folder, as this member sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoragePeerReport {
    pub device: DeviceId,
    /// Connected now: the rest is only asked of one that is.
    pub connected: bool,
    /// What it said of its room, if it said.
    pub usage: Option<StorageUsage>,
    /// By priority, highest first.
    pub levels: Vec<StorageLevel>,
    /// Why it said nothing, if it was asked.
    pub error: Option<String>,
}

/// What asking a storage peer to drop did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageDrop {
    pub dropped_chunks: u64,
    pub dropped_bytes: u64,
    /// Files that would be dropped with no copy known elsewhere; nothing is dropped for them
    /// unless forced.
    pub at_risk: Vec<String>,
    /// Whether it was asked: not if a file was at risk and it was not forced.
    pub done: bool,
}

/// An encrypted folder's storage priorities (ADR 0023): the rules in its
/// `.shoalpriorities`, and what they give each current file and directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Priorities {
    /// Each rule's priority and pattern, in the file's order; the last that matches wins.
    pub rules: Vec<(u8, String)>,
    /// The current files and directories, sorted by path.
    pub entries: Vec<RankedEntry>,
}

/// A file or directory's storage priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedEntry {
    pub path: String,
    pub directory: bool,
    /// 0 to 255, higher kept first; 0 never stored on a storage peer.
    pub priority: u8,
    /// The index in [`Priorities::rules`] of the rule that gives it, or none for the default.
    pub rule: Option<usize>,
}

/// A folder's rebuild (ADR 0034): its metadata was started over, from the files on disk and
/// what peers send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildInfo {
    /// When it began, in milliseconds since the Unix epoch.
    pub started_ms: i64,
    /// Why: the database was damaged or missing, or it was asked for.
    pub reason: String,
    /// When it finished, if it has: a peer's whole index arrived, or there was no peer.
    pub finished_ms: Option<i64>,
    /// While it has not: the connected peers sharing the folder, whose index it waits for.
    pub waiting_for: Vec<DeviceId>,
}

/// What a folder is doing (ADR 0030). The first that applies: error, rebuilding, paused,
/// scanning, syncing, idle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderState {
    /// Nothing to do.
    Idle,
    /// Looking for local changes.
    Scanning,
    /// Files are pending and a connected peer shares the folder.
    Syncing,
    /// Paused (ADR 0029).
    Paused,
    /// Its metadata is being started over, and it waits for a peer's index (ADR 0034).
    Rebuilding,
    /// The folder cannot be used, or its last scan failed.
    Error,
}

/// A folder's exchanges with one peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderPeer {
    /// The peer.
    pub device: DeviceId,
    /// When the folder last finished exchanging changes with it, since the engine started.
    pub last_exchange_ms: Option<i64>,
    /// Connected and caught up: the exchange finished, and later changes are pushed as they
    /// happen.
    pub up_to_date: bool,
}

/// What a failing path was being used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operation {
    /// Reading it for changes.
    Scan,
    /// Fetching its content from peers.
    Fetch,
    /// Writing it to disk.
    Write,
    /// Deleting it from disk.
    Delete,
}

/// Why a path failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemKind {
    /// Not allowed, including a read-only disk.
    PermissionDenied,
    /// No space left.
    DiskFull,
    /// No connected device has the content.
    ContentUnavailable,
    /// A name this device cannot sync, such as one that is not valid UTF-8.
    NameNotAllowed,
    /// The file changed while it was being read or written; tried again later.
    ChangedWhileWriting,
    /// Anything else; see the message.
    Other,
}

/// A path that failed, until it succeeds (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathProblem {
    /// The path in the folder.
    pub path: String,
    /// What it was being used for.
    pub operation: Operation,
    /// Why it failed.
    pub kind: ProblemKind,
    /// The error.
    pub message: String,
    /// When it first failed, in milliseconds since the Unix epoch.
    pub since_ms: i64,
    /// How many passes it has failed in a row.
    pub attempts: u32,
}

/// A device this one syncs with, connected or not (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerState {
    /// The device.
    pub device: DeviceId,
    /// Its name, if it has connected since the engine started.
    pub name: Option<String>,
    /// A sync session is running.
    pub connected: bool,
    /// `direct` or `relay`, while connected.
    pub path: Option<PathKind>,
    /// Round-trip time, while connected.
    pub rtt_ms: Option<u64>,
    /// When the current session started, in milliseconds since the Unix epoch.
    pub connected_since_ms: Option<i64>,
    /// When a session was last up: now while connected.
    pub last_seen_ms: Option<i64>,
    /// Why the last attempt to connect, or the last session, failed.
    pub last_error: Option<String>,
    /// How far its clock was ahead of this device's at the last handshake, in milliseconds.
    pub clock_offset_ms: Option<i64>,
}

/// One change this device applied to a folder, from its activity log (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityEntry {
    /// When this device applied it, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// What happened.
    pub kind: shoal_meta::ChangeKind,
    /// The entry's path.
    pub path: String,
    /// Its path before, for a rename.
    pub old_path: Option<String>,
    /// The device that made the change.
    pub device: DeviceId,
    /// That device's name, as far as this device knows it, or the start of its ID.
    pub device_name: String,
    /// Made on this device, rather than received from a peer.
    pub local: bool,
}

/// How a check of `shoal doctor` came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckLevel {
    /// Fine.
    Pass,
    /// Works, but worth attention.
    Warning,
    /// Something does not work.
    Failure,
}

/// One check of `shoal doctor` (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// What was checked: `state`, `clock`, `space`, `folder`, `filesystem`, `watch`, `relay`,
    /// `network` or `peers`.
    pub name: String,
    /// How it came out.
    pub level: CheckLevel,
    /// What was found.
    pub detail: String,
    /// What to do about it, for a warning or failure.
    pub advice: Option<String>,
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
    /// Path within the folder, where the file is on disk: for entries that share a path or
    /// whose names fold together, not always the path recorded (ADR 0024, ADR 0033).
    pub path: String,
    /// The current version's content.
    pub content: shoal_proto::ContentHash,
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
    /// That device's name, as far as this device knows it, or the start of its ID (ADR 0030).
    pub author_name: String,
    /// Full replicas that confirmed holding the current version's content (ADR 0022). Only
    /// tracked for content made on this device; 0 for everything else.
    pub confirmed_copies: usize,
    /// True if this device made the current version and no full replica has confirmed it:
    /// this device may hold the only copy.
    pub only_here: bool,
    /// Pinned on this device (ADR 0029): kept on disk and never evicted.
    pub pinned: bool,
    /// The file's ID on disk as last scanned, its inode or NTFS file ID; 0 if not on disk or
    /// not known.
    pub file_id: u64,
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
    /// True if this is a version that lost when a conflict was resolved, rather than a deleted
    /// file (ADR 0029). Restoring it makes it a file of its own beside `path`.
    pub conflict: bool,
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
    /// A file gained a version that conflicts with its current one, and the user has to decide
    /// which to keep (ADR 0029).
    ConflictDetected {
        /// The folder.
        folder: FolderId,
        /// The file's path.
        path: String,
    },
    /// A task panicked and the engine stopped working (ADR 0030).
    Crashed(String),
}

/// Which kind of version a [`VersionInfo`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionKind {
    /// The version the folder has now.
    Current,
    /// A version kept on this device after the engine replaced it (ADR 0029).
    Previous,
    /// A version made apart from the current one, waiting for the user to decide (ADR 0029).
    Conflict,
}

/// One version of a file (ADR 0029).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    /// Identifies the version for [`crate::Engine::restore_version`],
    /// [`crate::Engine::open_version`] and [`crate::Engine::resolve`]: `current`,
    /// `previous:<id>` or `conflict:<id>`.
    pub id: String,
    /// Which kind it is.
    pub kind: VersionKind,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, in milliseconds since the Unix epoch.
    pub modified_ms: i64,
    /// For the current version and conflicts, the device that made it.
    pub author: Option<DeviceId>,
    /// That device's name, as far as this device knows it.
    pub author_name: Option<String>,
    /// For a previous version, when this device kept it.
    pub kept_ms: Option<i64>,
    /// For a previous version, when it expires, if the folder's retention is not forever.
    pub expires_ms: Option<i64>,
}

/// What a revocation with a time of loss would withdraw in one folder, by this device's record
/// (ADR 0040).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossPreview {
    pub folder: FolderId,
    /// The time of loss as this folder applies it: no earlier than its retention allows.
    pub lost_ms: i64,
    /// True if the time given was earlier than that, and moved.
    pub moved: bool,
    /// The files that would go to the recycle bin.
    pub withdrawn: Vec<WithdrawnFile>,
    /// Versions the device made whose arrival this device cannot place before or after the
    /// time: left for the other replicas' judgement.
    pub unknown: usize,
}

/// A file a revocation would withdraw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WithdrawnFile {
    pub path: String,
    pub size: u64,
    /// When this device received the version, if it recorded it.
    pub received_ms: Option<i64>,
    /// What it replaced is put back, from this device's history; otherwise the file goes.
    pub put_back: bool,
}
