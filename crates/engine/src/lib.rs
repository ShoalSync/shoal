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
//! <folder root>/.tidefell/folder the folder's marker; without it the folder is unavailable
//! <folder root>/.tidefell/tmp/   in-progress downloads, renamed into place when complete
//! ```
//!
//! Folders and peers can also be added at runtime; they are remembered in `<data>/runtime`.
//!
//! Limitations: empty directories and symlinks are not synced, and conflict copies are kept in
//! metadata but not written to disk.

mod activity;
mod activity_log;
mod backend;
mod batch;
mod clock;
mod committer;
mod config;
mod content;
mod copies;
mod db;
pub mod device;

/// Creates the data directory, or the metadata directory, if it is not there, readable by its
/// owner alone, and makes an existing one so: keys, the control socket, the databases and the
/// logs live in them, and other users of the machine have no business with any of them.
pub fn create_data_dir(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(io_err(path))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(io_err(path))
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path).map_err(io_err(path))
}
pub mod diagnostics;
mod encrypted;
mod engine;
mod evict;
mod history;
mod ignores;
mod local;
mod loss;
mod manifests;
mod materialize;
pub mod pairing;
mod pins;
mod probe;
mod redact;
mod root;
mod runtime;
mod scan;
mod setaside;
mod sidecar;
mod space;
mod state;
mod stored;
#[doc(hidden)]
pub mod synth;
mod takeover;
mod tracked;

/// Entry points for the fuzz targets in `fuzz/` (ADR 0031), built only by `cargo fuzz`.
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
mod views;
pub(crate) mod vouch;
mod watch;

/// The allocator every program running the engine uses (ADR 0035): it returns freed memory to
/// the system and keeps less aside per thread than glibc's, which matters for a long-running
/// engine holding a large folder. Programs declare it with `#[global_allocator]`; a library
/// loaded at run time, as the Android app loads the FFI, needs the `dynamic-library` feature.
pub use mimalloc::MiMalloc as Allocator;

/// Settings [`Allocator`] reads from the environment when a program starts (ADR 0035). Freed
/// memory goes back to the system at once: by default only a second later, and only when the
/// allocator next runs, which an idle engine may not do for hours. Memory is committed as it is
/// used rather than a whole arena at a time. Together they take a third off a large folder's
/// resident memory. A program that cannot start with them, such as an app loading the engine
/// as a library, sets them before it loads it.
pub const ALLOCATOR_SETTINGS: [(&str, &str); 2] = [
    ("MIMALLOC_PURGE_DELAY", "0"),
    ("MIMALLOC_ARENA_EAGER_COMMIT", "0"),
];

/// Starts this program again with [`ALLOCATOR_SETTINGS`] if any is missing from its
/// environment, keeping its process ID and arguments. Returns if they are all set, or if it
/// cannot start again, which costs only memory.
pub fn with_allocator_settings() {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        if ALLOCATOR_SETTINGS
            .iter()
            .all(|(name, _)| std::env::var_os(name).is_some())
        {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let mut args = std::env::args_os();
        let mut command = std::process::Command::new(exe);
        if let Some(name) = args.next() {
            command.arg0(name);
        }
        for (name, value) in ALLOCATOR_SETTINGS {
            if std::env::var_os(name).is_none() {
                command.env(name, value);
            }
        }
        // Returns only if the program could not be started again.
        let _ = command.args(args).exec();
    }
}

pub use config::{
    ConfirmDeletes, Direction, Encryption, EngineConfig, FolderConfig, LocalDeletes, NewFolder,
    PeerAddr, ReplicaMode, Stored, SyncOptions,
};
pub use engine::{zip_bundle, BundleEntry, BundleOptions, Engine, FolderStatus, Resolution};
pub use probe::{Filesystem, NameLength};
pub use tidefell_identity::{DeviceKey, UserKey};
pub use tidefell_meta::{ChangeKind, Retention};
pub use tidefell_net::{
    Limits, PathKind, RelayConfig, RelayToken, RelayUrl, Transfer, MIN_BYTES_PER_SEC,
};
pub use tidefell_proto::identity::{DeviceCertificate, DeviceRevocation};
pub use tidefell_proto::{DeviceId, EntryId, FolderId, Signed, UserId};
pub use views::{
    ActivityEntry, Check, CheckLevel, CopyInfo, EncryptionInfo, Event, FileInfo, FolderInfo,
    FolderPeer, FolderState, LossPreview, Operation, PathProblem, PeerInfo, PeerState, ProblemKind,
    RebuildInfo, RecycleSummary, RecycledInfo, StoredInfo, VersionInfo, VersionKind, WithdrawnFile,
};

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
    /// The folders' databases are in the metadata directory used before, not the one set now
    /// (ADR 0034).
    #[error(
        "the folder metadata is in {from}, not in metadata_dir {to}; move it there, or set \
         metadata_dir back"
    )]
    MetadataLeftBehind {
        /// Where the databases are.
        from: std::path::PathBuf,
        /// Where they are expected.
        to: std::path::PathBuf,
    },
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
    /// A device cannot revoke itself (ADR 0040): revoke it from another device of the user.
    #[error("a device cannot revoke itself; revoke it from another of your devices")]
    RevokeSelf,
    /// A device is revoked once (ADR 0040): a time of loss is given when revoking it.
    #[error("that device is already revoked; a time of loss can only be given when revoking it")]
    AlreadyRevoked,
    /// A file on disk changed since the last scan, so it cannot be removed yet.
    #[error("{0} has changes that are not recorded yet; try again after the next scan")]
    LocalChanges(String),
    /// Something else is already at the path a file would be restored to.
    #[error("{0} is already taken")]
    PathTaken(String),
    /// A task panicked and the engine stopped working (ADR 0030).
    #[error("the engine stopped after an internal error: {0}")]
    Crashed(String),
    /// Syncing is paused for the folder or the whole device (ADR 0029).
    #[error("syncing of folder {0} is paused")]
    Paused(tidefell_proto::FolderId),
    /// A file's content could not be fetched from connected peers.
    #[error("{0} is not available: no connected peer has its content")]
    Unavailable(String),
    /// The folder's root or its marker is missing, as when its drive is not mounted, so
    /// nothing is scanned, written or deleted there (ADR 0037).
    #[error("folder {0} is unavailable: {1}")]
    FolderUnavailable(tidefell_proto::FolderId, String),
    /// A request was invalid.
    #[error("{0}")]
    Invalid(String),
    /// Randomness was unavailable.
    #[error("no randomness available")]
    Random,
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Frees the file at a path in place, as a Windows sync root dehydrates one (ADR 0042), if it
/// is still the given entry's content and no other program has it open: called with its
/// absolute path, entry and content, it returns whether it freed it. One that is not freed is
/// removed, as in any folder.
pub type Freer = std::sync::Arc<
    dyn Fn(&std::path::Path, EntryId, tidefell_proto::ContentHash) -> std::io::Result<bool>
        + Send
        + Sync,
>;

pub(crate) fn io_err(path: impl Into<std::path::PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}

/// Renames `from` to `to`, then syncs the directories holding them, so the rename survives a
/// power cut (ADR 0034). A filesystem that cannot sync a directory, as some FUSE mounts cannot,
/// keeps the rename without that guarantee.
pub(crate) fn rename_durably(
    from: impl AsRef<std::path::Path>,
    to: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let (from, to) = (from.as_ref(), to.as_ref());
    std::fs::rename(from, to).map_err(|e| busy_if_in_use(e, to))?;
    sync_dir_of(to);
    if from.parent() != to.parent() {
        sync_dir_of(from);
    }
    Ok(())
}

/// On Windows, a rename over a file another program holds open without letting it be deleted,
/// as some editors do, is refused as access denied, as missing permission is. Opening the file
/// for deletion tells them apart: that program's sharing refuses it. Such a file is busy, and
/// its replacement is tried again on the next pass rather than left as a lasting problem
/// (ADR 0042).
fn busy_if_in_use(error: std::io::Error, path: &std::path::Path) -> std::io::Error {
    if error.kind() == std::io::ErrorKind::PermissionDenied && in_use(path) {
        busy()
    } else {
        error
    }
}

/// Whether another program holds the file at `path` open without letting it be deleted, so it
/// cannot be replaced or moved until that program closes it. Only Windows has such files.
#[cfg(windows)]
pub(crate) fn in_use(path: &std::path::Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    const DELETE: u32 = 0x0001_0000;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    std::fs::OpenOptions::new()
        .access_mode(DELETE)
        .open(path)
        .is_err_and(|e| e.raw_os_error() == Some(ERROR_SHARING_VIOLATION))
}

#[cfg(not(windows))]
pub(crate) fn in_use(_path: &std::path::Path) -> bool {
    false
}

/// The error for a file [`in_use`]: tried again on the next pass.
pub(crate) fn busy() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::ResourceBusy,
        "the file is open in another program",
    )
}

/// Moves `from` to `to` as [`rename_durably`] does, but only if nothing is at `to`: the check
/// and the move are one step where the filesystem has hard links, so a file the user puts
/// there meanwhile is never replaced. Returns false, leaving `from`, if something is there.
pub(crate) fn rename_new_durably(
    from: impl AsRef<std::path::Path>,
    to: impl AsRef<std::path::Path>,
) -> std::io::Result<bool> {
    let (from, to) = (from.as_ref(), to.as_ref());
    match std::fs::hard_link(from, to) {
        Ok(()) => {
            // In place under its new name either way; a temporary name left behind goes with
            // the reserved directory's next clean-up.
            if let Err(e) = std::fs::remove_file(from) {
                tracing::debug!(path = %from.display(), error = %e, "cannot remove the old name");
            }
            sync_dir_of(to);
            if from.parent() != to.parent() {
                sync_dir_of(from);
            }
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        // No hard links, as on FAT: check, then move, which leaves a moment between the two.
        Err(_) => {
            if std::fs::symlink_metadata(to).is_ok() {
                return Ok(false);
            }
            rename_durably(from, to).map(|()| true)
        }
    }
}

/// Syncs the directory holding `path`, if the filesystem allows it.
pub(crate) fn sync_dir_of(path: &std::path::Path) {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::File::open(dir).and_then(|d| d.sync_all()) {
            tracing::debug!(dir = %dir.display(), error = %e, "cannot sync directory");
        }
    }
    #[cfg(not(unix))]
    let _ = path;
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

#[cfg(test)]
mod tests {
    use std::fs;

    #[test]
    fn a_rename_replaces_a_file_another_program_has_open() {
        let dir = tempfile::tempdir().unwrap();
        let (tmp, target) = (dir.path().join("new.tmp"), dir.path().join("a.txt"));
        fs::write(&target, b"old").unwrap();
        fs::write(&tmp, b"new").unwrap();
        // Open as programs usually open files: on Windows, sharing reads, writes and deletes.
        let _open = fs::File::open(&target).unwrap();
        super::rename_durably(&tmp, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert!(!tmp.exists());
    }

    #[test]
    fn a_new_name_never_replaces_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let (tmp, target) = (dir.path().join("new.tmp"), dir.path().join("a.txt"));
        fs::write(&target, b"theirs").unwrap();
        fs::write(&tmp, b"ours").unwrap();
        assert!(!super::rename_new_durably(&tmp, &target).unwrap());
        assert_eq!(fs::read(&target).unwrap(), b"theirs");
        assert_eq!(fs::read(&tmp).unwrap(), b"ours");

        fs::remove_file(&target).unwrap();
        assert!(super::rename_new_durably(&tmp, &target).unwrap());
        assert_eq!(fs::read(&target).unwrap(), b"ours");
        assert!(!tmp.exists());
    }

    /// A program that opens a file without sharing it, as some editors do, holds off its
    /// replacement until it closes it: the rename fails in a way tried again, not one that lasts.
    #[cfg(windows)]
    #[test]
    fn a_file_held_without_sharing_is_replaced_once_it_is_closed() {
        use std::os::windows::fs::OpenOptionsExt;

        let dir = tempfile::tempdir().unwrap();
        let (tmp, target) = (dir.path().join("new.tmp"), dir.path().join("a.txt"));
        fs::write(&target, b"old").unwrap();
        fs::write(&tmp, b"new").unwrap();
        let held = fs::File::options()
            .read(true)
            .share_mode(0)
            .open(&target)
            .unwrap();
        let error = super::rename_durably(&tmp, &target).unwrap_err();
        eprintln!(
            "rename over a held file: {error} ({:?}, {:?})",
            error.kind(),
            error.raw_os_error()
        );
        assert_eq!(error.kind(), std::io::ErrorKind::ResourceBusy, "{error}");
        assert!(!crate::activity::io_lasts(&error), "{error}");
        assert_eq!(fs::read(&tmp).unwrap(), b"new", "the new content is kept");
        drop(held);
        super::rename_durably(&tmp, &target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }
}
