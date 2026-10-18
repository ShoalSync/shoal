//! Per-folder engine state and its persistence.

use std::collections::{BTreeMap, BTreeSet};

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tidefell_identity::FolderMembers;
use tidefell_meta::{Folder, Recycled};
use tidefell_proto::sync::EntryKind;
use tidefell_proto::version::LegacyVersionVector;
use tidefell_proto::{ContentHash, DeviceId, EntryId, FolderId, Manifest, VersionVector};

use crate::committer::Committer;
use crate::config::FolderConfig;
use crate::db::{Changes, FolderDb, Rebuild};
use crate::history::Previous;
use crate::ignores::Ignores;
pub(crate) use crate::local::LocalFiles;
use crate::tracked::{Tracked, TrackedSet};
use crate::{io_err, random_bytes, Error, Result};

/// Where folder `id`'s database is: `<metadata_dir>/<id>.redb` (ADR 0034).
pub(crate) fn db_path(metadata_dir: &Path, id: FolderId) -> PathBuf {
    metadata_dir.join(format!("{id}.redb"))
}

/// How a device tells a folder's names apart (ADR 0033): in NFC, and without regard to case
/// on a drive that ignores it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NameFolding(pub(crate) tidefell_meta::Folding);

impl NameFolding {
    /// `name` as this device compares names.
    pub(crate) fn fold(self, name: &str) -> String {
        self.0.fold(&recorded(name)).into_owned()
    }
}

/// The path a name on disk is recorded as: its NFC form, as sync/1 requires (ADR 0033).
pub(crate) fn recorded(name: &str) -> std::borrow::Cow<'_, str> {
    use unicode_normalization::{is_nfc_quick, IsNormalized, UnicodeNormalization};
    match is_nfc_quick(name.chars()) {
        IsNormalized::Yes => std::borrow::Cow::Borrowed(name),
        _ => std::borrow::Cow::Owned(name.nfc().collect()),
    }
}

const STATE_HEADER: &[u8] = b"tidefell-engine-folder 9\n";
/// Format 7, before pins (ADR 0029).
/// Format 8, whose version vectors had no holes.
const STATE_HEADER_V8: &[u8] = b"tidefell-engine-folder 8\n";
const STATE_HEADER_V7: &[u8] = b"tidefell-engine-folder 7\n";
/// Format 6, before version history (ADR 0029).
const STATE_HEADER_V6: &[u8] = b"tidefell-engine-folder 6\n";
/// Before deleted files were kept on disk instead of in the block store (ADR 0027).
const STATE_HEADER_V5: &[u8] = b"tidefell-engine-folder 5\n";
/// Before files could be dropped from this device only or set aside (ADR 0025).
const STATE_HEADER_V4: &[u8] = b"tidefell-engine-folder 4\n";
/// Before files on disk recorded the version vector of their content (ADR 0024).
const STATE_HEADER_V3: &[u8] = b"tidefell-engine-folder 3\n";
/// Version 2 had no confirmed copies.
const STATE_HEADER_V2: &[u8] = b"tidefell-engine-folder 2\n";
/// Version 1 had no last-use times either.
const STATE_HEADER_V1: &[u8] = b"tidefell-engine-folder 1\n";

/// What this replica last saw or wrote at a path on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LocalFile {
    pub(crate) entry: EntryId,
    pub(crate) content: ContentHash,
    pub(crate) size: u64,
    /// Modification time in nanoseconds since the Unix epoch, as reported by the filesystem.
    pub(crate) mtime_ns: i128,
    pub(crate) executable: bool,
    /// The version vector this content corresponds to: a local change to it builds on this
    /// (ADR 0024). Empty if unknown, for files recorded before it was kept.
    pub(crate) base: VersionVector,
}

/// [`LocalFile`] in state formats 4 to 8, whose base had no holes.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct LocalFileV8 {
    entry: EntryId,
    content: ContentHash,
    size: u64,
    mtime_ns: i128,
    executable: bool,
    base: LegacyVersionVector,
}

impl From<LocalFileV8> for LocalFile {
    fn from(v8: LocalFileV8) -> Self {
        Self {
            entry: v8.entry,
            content: v8.content,
            size: v8.size,
            mtime_ns: v8.mtime_ns,
            executable: v8.executable,
            base: v8.base.into(),
        }
    }
}

/// [`SetAside`] in state formats 5 to 8.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct SetAsideV8 {
    path: String,
    file: String,
    local: LocalFileV8,
    removed_at_ms: i64,
}

impl From<SetAsideV8> for SetAside {
    fn from(v8: SetAsideV8) -> Self {
        Self {
            path: v8.path,
            file: v8.file,
            local: v8.local.into(),
            removed_at_ms: v8.removed_at_ms,
        }
    }
}

fn upgrade_local_v8(local: Vec<(String, LocalFileV8)>) -> Vec<(String, LocalFile)> {
    local.into_iter().map(|(p, l)| (p, l.into())).collect()
}

fn upgrade_set_aside(set_aside: Vec<(EntryId, SetAsideV8)>) -> Vec<(EntryId, SetAside)> {
    set_aside.into_iter().map(|(e, s)| (e, s.into())).collect()
}

/// [`LocalFile`] in state formats 1 to 3, which had no base.
#[derive(Debug, Clone, Copy, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct LocalFileV3 {
    entry: EntryId,
    content: ContentHash,
    size: u64,
    mtime_ns: i128,
    executable: bool,
}

impl From<LocalFileV3> for LocalFile {
    fn from(v3: LocalFileV3) -> Self {
        Self {
            entry: v3.entry,
            content: v3.content,
            size: v3.size,
            mtime_ns: v3.mtime_ns,
            executable: v3.executable,
            base: VersionVector::new(),
        }
    }
}

/// A file removed from this device only and kept in the folder's reserved directory until the
/// recycle bin purges it (ADR 0025). Restoring it moves it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SetAside {
    /// Where the file was in the folder.
    pub(crate) path: String,
    /// Its file name under the reserved recycle directory.
    pub(crate) file: String,
    /// What the engine knew of the file when it was removed.
    pub(crate) local: LocalFile,
    /// When it was removed, in milliseconds since the Unix epoch.
    pub(crate) removed_at_ms: i64,
}

fn upgrade_local(local: Vec<(String, LocalFileV3)>) -> Vec<(String, LocalFile)> {
    local.into_iter().map(|(p, l)| (p, l.into())).collect()
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFile)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
    copies: Vec<(ContentHash, DeviceId, i64)>,
    copy_names: Vec<(DeviceId, String)>,
    dropped: Vec<EntryId>,
    set_aside: Vec<(EntryId, SetAside)>,
    kept: Vec<(ContentHash, String)>,
    history: Vec<(EntryId, Vec<Previous>)>,
    pins: Vec<String>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct PersistedV8 {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFileV8)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
    copies: Vec<(ContentHash, DeviceId, i64)>,
    copy_names: Vec<(DeviceId, String)>,
    dropped: Vec<EntryId>,
    set_aside: Vec<(EntryId, SetAsideV8)>,
    kept: Vec<(ContentHash, String)>,
    history: Vec<(EntryId, Vec<Previous>)>,
    pins: Vec<String>,
}

impl From<PersistedV8> for Persisted {
    fn from(v8: PersistedV8) -> Self {
        Self {
            manifests: v8.manifests,
            local: upgrade_local_v8(v8.local),
            progress: v8.progress,
            used: v8.used,
            copies: v8.copies,
            copy_names: v8.copy_names,
            dropped: v8.dropped,
            set_aside: upgrade_set_aside(v8.set_aside),
            kept: v8.kept,
            history: v8.history,
            pins: v8.pins,
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct PersistedV7 {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFileV8)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
    copies: Vec<(ContentHash, DeviceId, i64)>,
    copy_names: Vec<(DeviceId, String)>,
    dropped: Vec<EntryId>,
    set_aside: Vec<(EntryId, SetAsideV8)>,
    kept: Vec<(ContentHash, String)>,
    history: Vec<(EntryId, Vec<Previous>)>,
}

impl From<PersistedV7> for Persisted {
    fn from(v7: PersistedV7) -> Self {
        Self {
            manifests: v7.manifests,
            local: upgrade_local_v8(v7.local),
            progress: v7.progress,
            used: v7.used,
            copies: v7.copies,
            copy_names: v7.copy_names,
            dropped: v7.dropped,
            set_aside: upgrade_set_aside(v7.set_aside),
            kept: v7.kept,
            history: v7.history,
            pins: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct PersistedV6 {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFileV8)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
    copies: Vec<(ContentHash, DeviceId, i64)>,
    copy_names: Vec<(DeviceId, String)>,
    dropped: Vec<EntryId>,
    set_aside: Vec<(EntryId, SetAsideV8)>,
    kept: Vec<(ContentHash, String)>,
}

impl From<PersistedV6> for Persisted {
    fn from(v6: PersistedV6) -> Self {
        Self {
            manifests: v6.manifests,
            local: upgrade_local_v8(v6.local),
            progress: v6.progress,
            used: v6.used,
            copies: v6.copies,
            copy_names: v6.copy_names,
            dropped: v6.dropped,
            set_aside: upgrade_set_aside(v6.set_aside),
            kept: v6.kept,
            history: Vec::new(),
            pins: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct PersistedV5 {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFileV8)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
    copies: Vec<(ContentHash, DeviceId, i64)>,
    copy_names: Vec<(DeviceId, String)>,
    dropped: Vec<EntryId>,
    set_aside: Vec<(EntryId, SetAsideV8)>,
}

impl From<PersistedV5> for Persisted {
    fn from(v5: PersistedV5) -> Self {
        Self {
            manifests: v5.manifests,
            local: upgrade_local_v8(v5.local),
            progress: v5.progress,
            used: v5.used,
            copies: v5.copies,
            copy_names: v5.copy_names,
            dropped: v5.dropped,
            set_aside: upgrade_set_aside(v5.set_aside),
            kept: Vec::new(),
            history: Vec::new(),
            pins: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct PersistedV4 {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFileV8)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
    copies: Vec<(ContentHash, DeviceId, i64)>,
    copy_names: Vec<(DeviceId, String)>,
}

impl From<PersistedV4> for Persisted {
    fn from(v4: PersistedV4) -> Self {
        Self {
            manifests: v4.manifests,
            local: upgrade_local_v8(v4.local),
            progress: v4.progress,
            used: v4.used,
            copies: v4.copies,
            copy_names: v4.copy_names,
            dropped: Vec::new(),
            set_aside: Vec::new(),
            kept: Vec::new(),
            history: Vec::new(),
            pins: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct PersistedV3 {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFileV3)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
    copies: Vec<(ContentHash, DeviceId, i64)>,
    copy_names: Vec<(DeviceId, String)>,
}

impl From<PersistedV3> for Persisted {
    fn from(v3: PersistedV3) -> Self {
        Self {
            manifests: v3.manifests,
            local: upgrade_local(v3.local),
            progress: v3.progress,
            used: v3.used,
            copies: v3.copies,
            copy_names: v3.copy_names,
            dropped: Vec::new(),
            set_aside: Vec::new(),
            kept: Vec::new(),
            history: Vec::new(),
            pins: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct PersistedV2 {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFileV3)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
}

impl From<PersistedV2> for Persisted {
    fn from(v2: PersistedV2) -> Self {
        Self {
            manifests: v2.manifests,
            local: upgrade_local(v2.local),
            progress: v2.progress,
            used: v2.used,
            copies: Vec::new(),
            copy_names: Vec::new(),
            dropped: Vec::new(),
            set_aside: Vec::new(),
            kept: Vec::new(),
            history: Vec::new(),
            pins: Vec::new(),
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct PersistedV1 {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFileV3)>,
    progress: Vec<(DeviceId, u64, u64)>,
}

impl From<PersistedV1> for Persisted {
    fn from(v1: PersistedV1) -> Self {
        Self {
            manifests: v1.manifests,
            local: upgrade_local(v1.local),
            progress: v1.progress,
            used: Vec::new(),
            copies: Vec::new(),
            copy_names: Vec::new(),
            dropped: Vec::new(),
            set_aside: Vec::new(),
            kept: Vec::new(),
            history: Vec::new(),
            pins: Vec::new(),
        }
    }
}

/// Everything the engine tracks for one folder.
/// How a folder is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Open {
    /// From its database, checked unless it is too large to check quickly (ADR 0035).
    Normal,
    /// From its database, checked whatever its size: it failed a check on its open handle.
    Recheck,
    /// Rebuilt, for this reason (ADR 0034).
    Rebuild(String),
}

pub(crate) struct FolderState {
    pub(crate) config: FolderConfig,
    pub(crate) members: FolderMembers,
    pub(crate) meta: Folder,
    /// Manifests and the chunk index, on disk (ADR 0035).
    pub(crate) manifests: crate::manifests::Manifests,
    /// Saves made, so changes held until committed are known to be (ADR 0035).
    saves: u64,
    /// Changes to the folder's state, so summaries of it are made again only after one.
    revision: u64,
    pub(crate) local: LocalFiles,
    pub(crate) progress: Tracked<DeviceId, (u64, u64)>,
    /// When each file on disk was last opened or written, in milliseconds since the Unix
    /// epoch, for evicting the least recently used files of a partial replica.
    pub(crate) used: Tracked<String, i64>,
    /// Confirmed copies (ADR 0022): for content this device made, the full replicas that
    /// reported holding all of it, and when, in milliseconds since the Unix epoch.
    pub(crate) copies: Tracked<ContentHash, BTreeMap<DeviceId, i64>>,
    /// Certificate names of the devices in `copies`, as they were when they confirmed.
    pub(crate) copy_names: Tracked<DeviceId, String>,
    /// Live entries dropped from this device only (ADR 0025): not written to disk again, even
    /// for new versions, until opened.
    pub(crate) dropped: TrackedSet<EntryId>,
    /// Files removed from this device only and kept for the recycle bin (ADR 0025).
    pub(crate) set_aside: Tracked<EntryId, SetAside>,
    /// Content deleted by other devices whose file was moved into the folder's reserved
    /// directory instead of being copied into the block store, by content: the file name there
    /// (ADR 0027). Removed once nothing references the content.
    pub(crate) kept: Tracked<ContentHash, String>,
    /// Previous versions of entries that the engine replaced on this device (ADR 0029), newest
    /// first after each purge.
    pub(crate) history: Tracked<EntryId, Vec<Previous>>,
    /// For each history file, the version that replaced its content: its author, when this
    /// device received it if that was known then, and its sequence, to read the time from its
    /// row later otherwise (ADR 0040).
    pub(crate) replaced: Tracked<String, (DeviceId, Option<i64>, u64)>,
    /// An encrypted folder's keys this device holds, by key ID: generation and key (ADR 0023).
    pub(crate) keys: Tracked<tidefell_proto::KeyId, (u64, [u8; 32])>,
    /// An encrypted folder's index items besides the versions in `meta`, by sequence:
    /// encryption keys, grants, and sealed versions this device cannot open yet.
    /// On a storage peer, the sealed versions among `items` that another kept version
    /// supersedes by vector, with when that was seen: no longer served, and dropped once the
    /// folder's retention has passed (ADR 0023).
    pub(crate) superseded: Tracked<u64, i64>,
    /// On a storage peer, the kept sealed versions no other supersedes, by entry, with their
    /// vectors: built from `items` when first needed, not stored.
    /// On a storage peer, the folder's sealed chunks (ADR 0023).
    pub(crate) sealed_store: Option<crate::stored::SealedStore>,
    pub(crate) stored_heads: Option<
        std::collections::HashMap<
            tidefell_proto::EntryId,
            Vec<(u64, tidefell_proto::VersionVector)>,
        >,
    >,
    pub(crate) items: Tracked<u64, tidefell_proto::sync2::Item>,
    /// Which chunk each of an encrypted folder's chunk IDs is, and under which key.
    pub(crate) chunk_ids:
        Tracked<tidefell_proto::ChunkId, (tidefell_proto::ChunkHash, tidefell_proto::KeyId)>,
    /// An encrypted folder's contents as its versions name them, by content and the key their
    /// chunks are sealed with (one content can be sealed under several): each chunk's ID and
    /// length, from which the manifest is made once fetched.
    pub(crate) sealed_chunks:
        Tracked<(ContentHash, tidefell_proto::KeyId), Vec<(tidefell_proto::ChunkId, u32)>>,
    /// Paths this device keeps on disk as a partial replica (ADR 0029): files, or directories
    /// and everything below them. Kept in the runtime settings, not the folder's database, so a
    /// rebuild keeps them (ADR 0034); a folder's metadata holds them only from before that.
    pub(crate) pins: BTreeSet<String>,
    /// Pins read from the database, which held them before the runtime settings did, to delete
    /// from it at the next save.
    old_pins: TrackedSet<String>,
    /// Deletes the last scan found and held for confirmation, because there were so many
    /// (ADR 0037): the paths, still recorded as on disk.
    pub(crate) held_deletes: Vec<String>,
    /// Set to let the next scan record the deletes it would hold.
    pub(crate) confirm_deletes: bool,
    /// This device's ignore patterns (ADR 0029), read again at every scan.
    pub(crate) ignores: Ignores,
    /// What the folder's filesystem does with names, probed when it opens (ADR 0033). Not
    /// saved: a folder that moved may be on another filesystem.
    pub(crate) filesystem: crate::probe::Filesystem,
    pub(crate) dir: PathBuf,
    /// The folder's database, in the metadata directory (ADR 0034).
    db_path: PathBuf,
    /// The recycle bin as the kept files' sidecars last recorded it, to rewrite them only when
    /// it changes (ADR 0034).
    pub(crate) sidecars_for: u64,
    /// The folder's last rebuild, if it had one (ADR 0034).
    rebuild: Option<Rebuild>,
    /// Commits the folder's changes to its database, in order, off the folder lock
    /// (ADR 0034). `None` once the folder is closed.
    committer: Option<Committer>,
    /// Set when the folder is removed; later saves are skipped so tasks still finishing their
    /// work cannot recreate its state directory.
    removed: bool,
    /// This device's time, the engine's (ADR 0036).
    pub(crate) clock: Arc<crate::clock::Clock>,
    /// True while the database, too large to check when opened, waits for its check.
    unchecked: bool,
}

impl FolderState {
    /// True if `folder` has saved state under `data_dir`, so it is not being created or joined.
    pub(crate) fn exists(data_dir: &Path, metadata_dir: &Path, folder: FolderId) -> bool {
        let dir = data_dir.join("folders").join(folder.to_string());
        FolderDb::exists(&db_path(metadata_dir, folder)) || dir.exists()
    }

    /// Loads the folder's state from `data_dir`, or creates it. A folder whose database is
    /// damaged, or missing though the folder existed, or that `rebuild` names a reason for, is
    /// rebuilt (ADR 0034).
    #[cfg(any(test, fuzzing))]
    pub(crate) fn open(
        data_dir: &Path,
        metadata_dir: &Path,
        config: FolderConfig,
        device: DeviceId,
    ) -> Result<Self> {
        Self::open_or_rebuild(data_dir, metadata_dir, config, device, Open::Normal)
    }

    /// Opens the folder as [`open`](Self::open) does, rebuilding it because of `rebuild` if
    /// that is given.
    pub(crate) fn open_or_rebuild(
        data_dir: &Path,
        metadata_dir: &Path,
        config: FolderConfig,
        device: DeviceId,
        open: Open,
    ) -> Result<Self> {
        let dir = data_dir.join("folders").join(config.id.to_string());
        let existed = dir.exists();
        fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        let path = db_path(metadata_dir, config.id);
        let members = FolderMembers::new(config.id, config.members.iter().copied());

        // A stored folder has no files, so nothing to ignore (ADR 0023).
        let ignores = if config.is_stored() {
            Ignores::defaults(&config.path)
        } else {
            Ignores::load(&config.path)
        };
        let retention = config.retention;
        let mut state = Self {
            meta: Folder::new(config.id, device, 1, retention),
            config,
            members,
            manifests: Default::default(),
            saves: 0,
            revision: 0,
            local: LocalFiles::default(),
            progress: Tracked::default(),
            used: Tracked::default(),
            copies: Tracked::default(),
            copy_names: Tracked::default(),
            dropped: TrackedSet::default(),
            set_aside: Tracked::default(),
            kept: Tracked::default(),
            history: Tracked::default(),
            replaced: Tracked::default(),
            keys: Tracked::default(),
            items: Tracked::default(),
            superseded: Tracked::default(),
            sealed_store: None,
            stored_heads: None,
            chunk_ids: Tracked::default(),
            sealed_chunks: Tracked::default(),
            pins: BTreeSet::new(),
            old_pins: TrackedSet::default(),
            held_deletes: Vec::new(),
            confirm_deletes: false,
            ignores,
            filesystem: crate::probe::Filesystem::default(),
            dir,
            db_path: path,
            sidecars_for: 0,
            committer: None,
            rebuild: None,
            removed: false,
            clock: Arc::default(),
            unchecked: false,
        };
        let has_db = FolderDb::exists(&state.db_path);
        let has_files = state.dir.join("meta").exists() || state.dir.join("state").exists();
        let loaded = match open.clone() {
            Open::Rebuild(reason) => Err(reason),
            Open::Normal | Open::Recheck if has_db => match state.load(open == Open::Recheck) {
                Ok(db) => Ok(db),
                Err(Error::CorruptState(_)) => Err("its metadata database was damaged".into()),
                Err(e) => return Err(e),
            },
            _ if !has_files && existed => Err("its metadata database was missing".into()),
            _ => {
                // A new folder, or one saved as files by an earlier version: those are
                // converted, then deleted (ADR 0034).
                state.load_files(device)?;
                Ok(state.create_db()?)
            }
        };
        let db = match loaded {
            Ok(db) => db,
            Err(reason) => {
                tracing::warn!(folder = %state.config.id, %reason, "rebuilding the folder");
                state.rebuild_from_scratch(device, reason)?
            }
        };
        if has_files {
            // A new folder, or one saved as files by an earlier version: those are converted,
            // then deleted (ADR 0034).
            for name in ["meta", "state"] {
                let path = state.dir.join(name);
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io_err(&path)(e)),
                }
            }
        }
        state.meta.set_retention(retention);
        let lines = crate::activity_log::compact(&state.dir, crate::now_ms()).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "compacting the activity log failed");
            0
        });
        state.committer = Some(Committer::start(
            db,
            state.meta.sequence(),
            state.dir.clone(),
            lines,
        ));
        // Files kept by earlier versions have no sidecar yet (ADR 0034).
        if state.config.is_stored() {
            state.sealed_store = Some(crate::stored::SealedStore::open(&state.config.path)?);
        } else {
            state.refresh_sidecars();
        }
        Ok(state)
    }

    /// Creates the folder's database and writes all of the folder to it.
    fn create_db(&mut self) -> Result<FolderDb> {
        let mut db = FolderDb::create(&self.db_path, self.config.encryption.is_some())?;
        let (manifests, chunks) = self.manifests.all();
        let mut maps = self.all_maps();
        maps.push(("manifests", manifests));
        db.commit(&Changes {
            header: self.meta.header(),
            journal: self.meta.full_journal(),
            maps,
            durable: true,
            rebuild: self.rebuild.clone(),
            activity: Vec::new(),
            at_ms: 0,
            chunks,
            generation: 0,
        })?;
        // All of it is in the database now.
        self.manifests = Default::default();
        self.manifests.set_reader(db.reader());
        Ok(db)
    }

    /// Starts the folder over, because of `reason` (ADR 0034): a damaged database is moved
    /// aside as `<id>.corrupt-<time>`, and a new one gets a new index epoch, so every peer sends
    /// its whole index again, and the recycle bin, history and files set aside on this device
    /// back from their sidecars once its root is there. Files on disk are matched to entries by
    /// content as versions arrive; until a peer's whole index is here, the folder is rebuilding.
    fn rebuild_from_scratch(&mut self, device: DeviceId, reason: String) -> Result<FolderDb> {
        let now = self.now_ms();
        if self.db_path.exists() {
            let aside = self
                .db_path
                .with_file_name(format!("{}.corrupt-{now}", self.config.id));
            crate::rename_durably(&self.db_path, &aside).map_err(io_err(&self.db_path))?;
        }
        let epoch = u64::from_le_bytes(random_bytes()?).max(1);
        self.meta = Folder::new(self.config.id, device, epoch, self.config.retention);
        self.manifests = Default::default();
        self.local = LocalFiles::default();
        self.progress = Tracked::default();
        self.used = Tracked::default();
        self.copies = Tracked::default();
        self.copy_names = Tracked::default();
        self.dropped = TrackedSet::default();
        self.set_aside = Tracked::default();
        self.kept = Tracked::default();
        self.history = Tracked::default();
        self.rebuild = Some(Rebuild {
            started_ms: now,
            reason,
            restored: false,
            finished_ms: None,
        });
        self.create_db()
    }

    /// True while the folder is being rebuilt (ADR 0034): it matches files on disk to the
    /// entries peers send, records no deletes, and tells peers it is not a full replica.
    /// Milliseconds since the Unix epoch, as this device tells it.
    pub(crate) fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// True if the database was too large to check when it was opened, and is not checked yet
    /// (ADR 0035).
    pub(crate) fn unchecked(&self) -> bool {
        self.unchecked
    }

    /// Notes that the database was checked.
    pub(crate) fn mark_checked(&mut self) {
        self.unchecked = false;
    }

    pub(crate) fn rebuilding(&self) -> bool {
        self.rebuild
            .as_ref()
            .is_some_and(|r| r.finished_ms.is_none())
    }

    /// The folder's last rebuild, if it had one.
    pub(crate) fn last_rebuild(&self) -> Option<&Rebuild> {
        self.rebuild.as_ref()
    }

    /// Gives a folder being rebuilt its recycle bin, history and files set aside back from
    /// their sidecars, once. Callers have checked that the folder's root is there.
    pub(crate) fn restore_if_rebuilding(&mut self) {
        if self.rebuilding() && self.rebuild.as_ref().is_some_and(|r| !r.restored) {
            crate::sidecar::restore(self);
            if let Some(rebuild) = self.rebuild.as_mut() {
                rebuild.restored = true;
            }
        }
    }

    /// True once a peer's whole index has arrived since the folder was rebuilt.
    pub(crate) fn exchanged(&self) -> bool {
        !self.progress.is_empty()
    }

    /// Ends the rebuild once what was kept is restored and a peer's whole index was here when
    /// the scan calling this began, so every file it found was matched (`exchanged`), or there
    /// is no peer to wait for (`alone`). Returns true if it ended.
    pub(crate) fn finish_rebuild(&mut self, alone: bool, exchanged: bool, now_ms: i64) -> bool {
        match self.rebuild.as_mut() {
            Some(r) if r.finished_ms.is_none() && r.restored && (alone || exchanged) => {
                r.finished_ms = Some(now_ms);
                tracing::info!(folder = %self.config.id, "folder rebuilt");
                true
            }
            _ => false,
        }
    }

    /// Loads the folder's database, and returns it to commit to.
    fn load(&mut self, check: bool) -> Result<FolderDb> {
        let (db, loaded) = FolderDb::open(&self.db_path, check)?;
        self.unchecked = !db.checked();
        let corrupt = |_| Error::CorruptState(self.db_path.clone());
        let mut maps = loaded.maps;
        let mut take = |name: &str| maps.remove(name).unwrap_or_default();
        self.rebuild = loaded.rebuild;
        self.meta = Folder::from_records(loaded.header, loaded.versions, loaded.recycled)
            .map_err(|_| Error::CorruptState(self.db_path.clone()))?;
        // Manifests are read from the database as needed (ADR 0035).
        self.manifests = Default::default();
        self.manifests.set_reader(db.reader());
        let meta = &self.meta;
        self.local = LocalFiles::decode(take("local"), |entry| {
            meta.current(entry).map(|v| v.shared_path())
        })
        .map_err(corrupt)?;
        self.progress = Tracked::decode(take("progress")).map_err(corrupt)?;
        self.used = Tracked::decode(take("used")).map_err(corrupt)?;
        self.copies = Tracked::decode(take("copies")).map_err(corrupt)?;
        self.copy_names = Tracked::decode(take("copy_names")).map_err(corrupt)?;
        self.dropped = TrackedSet::decode(take("dropped_entries")).map_err(corrupt)?;
        self.set_aside = Tracked::decode(take("set_aside")).map_err(corrupt)?;
        self.kept = Tracked::decode(take("kept")).map_err(corrupt)?;
        self.history = Tracked::decode(take("history")).map_err(corrupt)?;
        self.replaced = Tracked::decode(take("replaced")).map_err(corrupt)?;
        self.keys = Tracked::decode(take("keys")).map_err(corrupt)?;
        self.items = Tracked::decode(take("items")).map_err(corrupt)?;
        self.superseded = Tracked::decode(take("superseded")).map_err(corrupt)?;
        self.chunk_ids = Tracked::decode(take("chunk_ids")).map_err(corrupt)?;
        self.sealed_chunks = Tracked::decode(take("sealed_chunks")).map_err(corrupt)?;
        // A folder is encrypted or not from its creation (ADR 0023): a database that disagrees
        // with the configuration belongs to another folder of the same ID.
        if db.encrypted() != self.config.encryption.is_some() {
            return Err(Error::Invalid(format!(
                "folder {}: its metadata is {} but it is configured {}",
                self.config.id,
                if db.encrypted() {
                    "encrypted"
                } else {
                    "plaintext"
                },
                if db.encrypted() {
                    "plaintext"
                } else {
                    "encrypted"
                },
            )));
        }
        let mut old_pins: TrackedSet<String> = TrackedSet::decode(take("pins")).map_err(corrupt)?;
        self.pins = old_pins.iter().cloned().collect();
        for path in &self.pins {
            old_pins.remove(path);
        }
        self.old_pins = old_pins;
        Ok(db)
    }

    /// Loads the folder's `meta` and `state` files, as earlier versions saved them, or starts
    /// a new folder if there are none.
    fn load_files(&mut self, device: DeviceId) -> Result<()> {
        let meta_path = self.dir.join("meta");
        self.meta = if meta_path.exists() {
            Folder::load(&meta_path)?
        } else {
            let epoch = u64::from_le_bytes(random_bytes()?).max(1);
            Folder::new(self.config.id, device, epoch, self.config.retention)
        };
        let state = self;
        let state_path = state.dir.join("state");
        if state_path.exists() {
            let bytes = fs::read(&state_path).map_err(io_err(&state_path))?;
            let upgrading = !bytes.starts_with(STATE_HEADER);
            let persisted: Persisted = if let Some(body) = bytes.strip_prefix(STATE_HEADER) {
                postcard::from_bytes(body).ok()
            } else if let Some(body) = bytes.strip_prefix(STATE_HEADER_V8) {
                postcard::from_bytes::<PersistedV8>(body)
                    .ok()
                    .map(Into::into)
            } else if let Some(body) = bytes.strip_prefix(STATE_HEADER_V7) {
                postcard::from_bytes::<PersistedV7>(body)
                    .ok()
                    .map(Into::into)
            } else if let Some(body) = bytes.strip_prefix(STATE_HEADER_V6) {
                postcard::from_bytes::<PersistedV6>(body)
                    .ok()
                    .map(Into::into)
            } else if let Some(body) = bytes.strip_prefix(STATE_HEADER_V5) {
                postcard::from_bytes::<PersistedV5>(body)
                    .ok()
                    .map(Into::into)
            } else if let Some(body) = bytes.strip_prefix(STATE_HEADER_V4) {
                postcard::from_bytes::<PersistedV4>(body)
                    .ok()
                    .map(Into::into)
            } else if let Some(body) = bytes.strip_prefix(STATE_HEADER_V3) {
                postcard::from_bytes::<PersistedV3>(body)
                    .ok()
                    .map(Into::into)
            } else if let Some(body) = bytes.strip_prefix(STATE_HEADER_V2) {
                postcard::from_bytes::<PersistedV2>(body)
                    .ok()
                    .map(Into::into)
            } else {
                bytes
                    .strip_prefix(STATE_HEADER_V1)
                    .and_then(|body| postcard::from_bytes::<PersistedV1>(body).ok())
                    .map(Into::into)
            }
            .ok_or_else(|| Error::CorruptState(state_path.clone()))?;
            for manifest in persisted.manifests {
                state.add_manifest(manifest);
            }
            state.local = persisted.local.into_iter().collect();
            state.progress = persisted
                .progress
                .into_iter()
                .map(|(d, e, s)| (d, (e, s)))
                .collect();
            state.used = persisted.used.into_iter().collect();
            for (content, device, at) in persisted.copies {
                state.copies.entry(content).or_default().insert(device, at);
            }
            state.copy_names = persisted.copy_names.into_iter().collect();
            state.dropped = persisted.dropped.into_iter().collect();
            state.set_aside = persisted.set_aside.into_iter().collect();
            state.kept = persisted.kept.into_iter().collect();
            state.history = persisted.history.into_iter().collect();
            state.pins = persisted.pins.into_iter().collect();
            if upgrading {
                state.catch_up_bases();
            }
        }
        Ok(())
    }

    /// Every map the database holds, encoded, as if all of it had changed.
    fn all_maps(&self) -> Vec<(&'static str, Vec<crate::tracked::Changed>)> {
        vec![
            ("local", self.local.all()),
            ("progress", self.progress.all()),
            ("used", self.used.all()),
            ("copies", self.copies.all()),
            ("copy_names", self.copy_names.all()),
            ("dropped_entries", self.dropped.all()),
            ("set_aside", self.set_aside.all()),
            ("kept", self.kept.all()),
            ("history", self.history.all()),
            ("replaced", self.replaced.all()),
            ("keys", self.keys.all()),
            ("items", self.items.all()),
            ("superseded", self.superseded.all()),
            ("chunk_ids", self.chunk_ids.all()),
            ("sealed_chunks", self.sealed_chunks.all()),
            ("pins", self.old_pins.all()),
        ]
    }

    /// What changed in each map since the last save, encoded.
    fn changed_maps(&mut self) -> Vec<(&'static str, Vec<crate::tracked::Changed>)> {
        vec![
            ("local", self.local.take_changed()),
            ("progress", self.progress.take_changed()),
            ("used", self.used.take_changed()),
            ("copies", self.copies.take_changed()),
            ("copy_names", self.copy_names.take_changed()),
            ("dropped_entries", self.dropped.take_changed()),
            ("set_aside", self.set_aside.take_changed()),
            ("kept", self.kept.take_changed()),
            ("history", self.history.take_changed()),
            ("replaced", self.replaced.take_changed()),
            ("keys", self.keys.take_changed()),
            ("items", self.items.take_changed()),
            ("superseded", self.superseded.take_changed()),
            ("chunk_ids", self.chunk_ids.take_changed()),
            ("sealed_chunks", self.sealed_chunks.take_changed()),
            ("pins", self.old_pins.take_changed()),
        ]
    }

    /// How this device tells the folder's names apart (ADR 0033).
    pub(crate) fn folding(&self) -> NameFolding {
        NameFolding(match self.filesystem.case_insensitive {
            true => tidefell_meta::Folding::Case,
            false => tidefell_meta::Folding::None,
        })
    }

    /// Records what the folder's drive does with names (ADR 0033), which decides where entries
    /// whose names fold together are written.
    pub(crate) fn set_filesystem(&mut self, filesystem: crate::probe::Filesystem) {
        self.touch();
        self.filesystem = filesystem;
        let folding = self.folding().0;
        self.meta.set_folding(folding);
    }

    /// True if the file on disk at `disk` is the one recorded at `path`: the names are the same
    /// as this device tells names apart (ADR 0033).
    pub(crate) fn same_place(&self, disk: &str, path: &str) -> bool {
        disk == path || self.folding().fold(disk) == self.folding().fold(path)
    }

    /// Where the file recorded at `path` is on disk: the name of a file already there that is
    /// at `path` as this device tells names apart, which keeps the spelling it has on disk
    /// (ADR 0033), or else `path`.
    pub(crate) fn on_disk(&self, path: &str) -> String {
        if self.local.contains_key(path) || (path.is_ascii() && !self.filesystem.case_insensitive) {
            return path.to_owned();
        }
        self.local
            .keys()
            .find(|disk| self.same_place(disk, path))
            .map(|p| p.to_string())
            .unwrap_or_else(|| path.to_owned())
    }

    /// Saves the folder and closes its database, so another engine in this process can open
    /// it, as Android's does when it restarts. Later saves do nothing.
    pub(crate) fn close(&mut self) -> Result<()> {
        self.save()?;
        let closed = match self.committer.take() {
            Some(committer) => committer.close(),
            None => Ok(()),
        };
        self.manifests.close();
        closed
    }

    /// Brings the base of each file on disk that is its current version up to that version's
    /// vector (ADR 0029). The file already is that content, so it corresponds to every version
    /// with that content and every settled delete: a resolve another device made to keep this
    /// content, or a same-content version that arrived later, is never written, yet the next
    /// edit here must supersede it. Bases recorded before vectors had holes also lag behind this
    /// device's own later changes, such as renames. Files with other content keep their base,
    /// so a remote version never written to disk still conflicts with their changes.
    pub(crate) fn catch_up_bases(&mut self) {
        let mut caught_up = Vec::new();
        for (path, local) in self.local.iter() {
            let current = self.meta.current(&local.entry).is_some_and(|v| {
                self.corresponds(path, local.entry, local.content, local.executable, v.kind)
            });
            if current {
                let vector = self.meta.current_vector(&local.entry);
                if !local.base().dominates(&vector) {
                    let inode = local.inode;
                    let mut local = local.to_file();
                    local.base.merge(&vector);
                    caught_up.push((path.clone(), local, inode));
                }
            }
        }
        // The same file, still known by its inode: without it a scan tells a file replaced by
        // one of the same size and time from it only by reading it.
        for (path, local, inode) in caught_up {
            self.record_local(path.to_string(), local);
            self.local.set_inode(&path, inode);
        }
    }

    /// True if the file recorded at `path`, as `entry` with `content`, is `kind` where `entry`
    /// is on disk: the file a version of that kind is, which [`catch_up_bases`] brings its
    /// base up to.
    ///
    /// [`catch_up_bases`]: Self::catch_up_bases
    fn corresponds(
        &self,
        path: &str,
        entry: EntryId,
        content: ContentHash,
        executable: bool,
        kind: &EntryKind,
    ) -> bool {
        matches!(kind, EntryKind::File { content: c, executable: x, .. }
            if *c == content && *x == executable)
            && self
                .meta
                .disk_path(&entry)
                .is_some_and(|p| self.same_place(path, p))
    }

    /// True if a file on disk is `entry` as `kind`: one [`catch_up_bases`] would bring up to a
    /// version of that kind. The same content elsewhere, under another entry or a path this
    /// replica has not moved it to, is not that version.
    ///
    /// [`catch_up_bases`]: Self::catch_up_bases
    pub(crate) fn on_disk_as(&self, entry: &EntryId, kind: &EntryKind) -> bool {
        self.local.with_entry(entry).any(|(path, local)| {
            self.corresponds(path, *entry, local.content, local.executable, kind)
        })
    }

    /// Marks the folder removed and returns its state directory for deletion.
    pub(crate) fn mark_removed(&mut self) -> PathBuf {
        self.removed = true;
        self.committer = None;
        self.manifests.close();
        self.dir.clone()
    }

    /// Writes metadata and engine state atomically.
    /// Records the file at `path`, sharing its path with its entry's version when they are the
    /// same (ADR 0035).
    pub(crate) fn record_local(&mut self, path: String, file: LocalFile) -> Option<LocalFile> {
        let shared = self.meta.current(&file.entry).map(|v| v.shared_path());
        self.local.insert_shared(path, file, shared)
    }

    /// Which change the folder's state is at: different after any change that a status could
    /// show.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// Notes a change made without a save, such as to the pins.
    pub(crate) fn touch(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub(crate) fn save(&mut self) -> Result<()> {
        // Anything saved may change what the folder's status shows.
        self.touch();
        // A removed folder's state directory is gone, and a closed one belongs to the next
        // engine: tasks still finishing their work must not write to either.
        if self.removed || self.committer.is_none() {
            return Ok(());
        }
        // Only what changed, committed in order off the folder lock (ADR 0034). Last-use times
        // alone are not worth waiting for the disk.
        let journal = self.meta.take_journal();
        let mut maps = self.changed_maps();
        self.saves += 1;
        let (manifests, chunks) = self.manifests.take_changed(self.saves);
        maps.push(("manifests", manifests));
        // A file kept for the recycle bin before its item existed, as a delete made here keeps
        // it, gets a sidecar naming the item now, not at the next tidy (ADR 0034). Only a save
        // that changed what sidecars say looks.
        let kept_changed = maps.iter().any(|(name, records)| {
            matches!(*name, "kept" | "history" | "set_aside") && !records.is_empty()
        });
        if kept_changed || !journal.recycle.is_empty() {
            self.refresh_sidecars();
        }
        let durable = !journal.is_empty()
            || maps
                .iter()
                .any(|(name, records)| *name != "used" && !records.is_empty());
        let changes = Changes {
            header: self.meta.header(),
            journal,
            maps,
            durable,
            rebuild: self.rebuild.clone(),
            // What the saved changes did, for the activity log (ADR 0030), which the committer
            // writes too.
            activity: self.meta.take_changes(),
            at_ms: self.now_ms(),
            chunks,
            generation: self.saves,
        };
        if let Some(committer) = &self.committer {
            committer.submit(changes);
            // What earlier saves carried and the database has, is read from it now.
            self.manifests.committed(committer.committed_generation());
        }
        #[cfg(debug_assertions)]
        self.check_saved();
        Ok(())
    }

    /// The last sequence whose versions are on disk: peers are sent nothing later (ADR 0034).
    pub(crate) fn committed(&self) -> u64 {
        self.committer
            .as_ref()
            .map_or(self.meta.sequence(), Committer::committed)
    }

    /// The committer, to wait for what was saved so far to be on disk, without the folder's
    /// lock held.
    pub(crate) fn committer(&self) -> Option<&Committer> {
        self.committer.as_ref()
    }

    /// Why the folder's metadata cannot be saved, while it cannot (ADR 0037). Remote changes
    /// are not applied and peers' progress is not recorded meanwhile, so nothing this device
    /// cannot keep is acknowledged, and peers send it all again once saving works.
    pub(crate) fn save_failed(&self) -> Option<String> {
        self.committer.as_ref().and_then(Committer::failed)
    }

    /// Rewrites the sidecars of kept files if the recycle bin changed since they were last
    /// written, and writes any that are missing.
    pub(crate) fn refresh_sidecars(&mut self) {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for item in self.meta.recycle_bin().iter() {
            (item.version.entry, item.deleted_at_ms).hash(&mut hasher);
        }
        (self.kept.len(), self.history.len(), self.set_aside.len()).hash(&mut hasher);
        let fingerprint = hasher.finish().max(1);
        if fingerprint != self.sidecars_for {
            crate::sidecar::refresh(self);
            self.sidecars_for = fingerprint;
        }
    }

    /// Calls `on_commit` after each of the folder's commits.
    pub(crate) fn on_commit(&self, on_commit: crate::committer::OnCommit) {
        if let Some(committer) = &self.committer {
            committer.on_commit(on_commit);
        }
    }

    /// In development builds, that the database holds exactly what is in memory, for folders
    /// small enough to read back after every save: a change made without being recorded would
    /// otherwise be lost only at the next start.
    #[cfg(debug_assertions)]
    fn check_saved(&self) {
        if self.meta.entries().nth(2000).is_some() {
            return;
        }
        let Some(committer) = &self.committer else {
            return;
        };
        // A commit that fails is retried and reported (ADR 0037); there is nothing to compare yet.
        if committer.flush().is_err() {
            return;
        }
        let mut loaded = committer.read_all().expect("the database reads back");
        let sorted = |mut records: Vec<(Vec<u8>, Vec<u8>)>| {
            records.sort();
            records
        };
        for (name, records) in self.all_maps() {
            let in_memory: Vec<(Vec<u8>, Vec<u8>)> = records
                .into_iter()
                .filter_map(|(k, v)| Some((k, v?)))
                .collect();
            let saved = loaded.maps.remove(name).unwrap_or_default();
            assert_eq!(
                sorted(saved),
                sorted(in_memory),
                "the {name} table differs from memory"
            );
        }
        assert_eq!(loaded.header, self.meta.header(), "the header differs");
        let saved: Vec<(u64, tidefell_proto::EntryId)> = loaded
            .versions
            .iter()
            .map(|(s, v, _)| (*s, tidefell_meta::copy_id(v)))
            .collect();
        assert_eq!(
            saved,
            self.meta.stored_ids(),
            "the stored versions differ from memory"
        );
        let mut saved: Vec<String> = loaded.recycled.iter().map(|r| format!("{r:?}")).collect();
        let mut in_memory: Vec<String> = self
            .meta
            .recycle_bin()
            .iter()
            .map(|r| format!("{r:?}"))
            .collect();
        saved.sort();
        in_memory.sort();
        assert_eq!(saved, in_memory, "the recycle bin differs from memory");
    }

    /// Replaces the member list.
    pub(crate) fn set_members(&mut self, members: Vec<tidefell_proto::UserId>) {
        self.members = FolderMembers::new(self.config.id, members.iter().copied());
        self.config.members = members;
        self.touch();
    }

    pub(crate) fn add_manifest(&mut self, manifest: Manifest) {
        self.manifests.insert(manifest);
    }

    /// Forgets a manifest that turned out to be wrong, so it is fetched again.
    pub(crate) fn remove_manifest(&mut self, content: &ContentHash) {
        if self.manifests.contains_key(content) {
            self.manifests.remove(content);
        }
    }

    /// Content hashes still needed: current versions, conflict copies and recycle-bin items.
    pub(crate) fn referenced_contents(&self) -> std::collections::HashSet<ContentHash> {
        let mut out = std::collections::HashSet::new();
        let mut add = |kind: &EntryKind| {
            if let EntryKind::File { content, .. } = kind {
                out.insert(*content);
            }
        };
        for version in self.meta.entries() {
            add(version.kind);
            for conflict in self.meta.conflicts(&version.entry) {
                add(&conflict.version.kind);
            }
        }
        for item in self.meta.recycle_bin().iter() {
            add(&item.version.kind);
        }
        out.extend(self.history.values().flatten().map(|v| v.content));
        out
    }

    /// Changes the recycle-bin retention. Expired items are purged at the next purge.
    pub(crate) fn set_retention(&mut self, retention: tidefell_meta::Retention) {
        self.meta.set_retention(retention);
        self.config.retention = retention;
        self.touch();
    }

    /// Purges expired recycle-bin items and forgets manifests nothing references. Returns
    /// chunks no longer referenced by this folder.
    pub(crate) fn purge(&mut self, now_ms: i64) -> (Vec<Recycled>, Vec<tidefell_proto::ChunkHash>) {
        let purged = self.meta.purge_expired(now_ms);
        crate::history::purge(self, now_ms);
        (purged, self.release_unreferenced())
    }

    /// Removes `entry`, or every item if `None`, from the recycle bin and forgets manifests
    /// nothing references. Returns the removed items and chunks no longer referenced by this
    /// folder.
    pub(crate) fn discard(
        &mut self,
        entry: Option<&tidefell_proto::EntryId>,
    ) -> (Vec<Recycled>, Vec<tidefell_proto::ChunkHash>) {
        let discarded = match entry {
            Some(entry) => self.meta.discard(entry).into_iter().collect(),
            None => self.meta.discard_all(),
        };
        (discarded, self.release_unreferenced())
    }

    /// Forgets manifests nothing references and returns their chunks.
    fn release_unreferenced(&mut self) -> Vec<tidefell_proto::ChunkHash> {
        let referenced = self.referenced_contents();
        let unused: Vec<ContentHash> = self
            .manifests
            .contents()
            .into_iter()
            .filter(|c| !referenced.contains(c))
            .collect();
        self.copies.retain(|c, _| referenced.contains(c));
        let still_named: std::collections::HashSet<DeviceId> = self
            .copies
            .values()
            .flat_map(|d| d.keys().copied())
            .collect();
        self.copy_names.retain(|d, _| still_named.contains(d));
        // While the folder is rebuilt, what is kept is needed by versions that may not have
        // arrived.
        let unkept: Vec<ContentHash> = self
            .kept
            .keys()
            .filter(|c| !self.rebuilding() && !referenced.contains(c))
            .copied()
            .collect();
        for content in unkept {
            crate::content::release_kept(self, content);
        }
        let mut orphaned = Vec::new();
        for content in unused {
            orphaned.extend(self.manifests.remove(&content));
        }
        orphaned
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// ADR 0034's exit criterion: at 100,000 entries, a batch of 512 versions from a peer
    /// writes under 1 MB and holds the folder lock for under 50 ms. Slow, so only when asked:
    /// `cargo test --release -p tidefell-engine --lib -- --ignored a_batch_at_100000_entries`.
    #[test]
    #[ignore = "a measurement; run it in release with --ignored"]
    fn a_batch_at_100000_entries() {
        use tidefell_identity::DeviceKey;
        const ENTRIES: usize = 100_000;
        const BATCH: usize = 512;
        const ROUNDS: usize = 20;
        let written = || {
            fs::read_to_string("/proc/self/io")
                .ok()
                .and_then(|io| {
                    io.lines()
                        .find_map(|l| l.strip_prefix("write_bytes: ")?.trim().parse::<u64>().ok())
                })
                .unwrap_or(0)
        };
        let base = std::env::current_dir().unwrap().join("target");
        fs::create_dir_all(&base).unwrap();
        let tmp = tempfile::tempdir_in(&base).unwrap();
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: tmp.path().join("folder"),
            mode: ReplicaMode::Full,
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        };
        let mine = DeviceKey::from_seed([5; 32]);
        let other = DeviceKey::from_seed([6; 32]);
        let mut state =
            FolderState::open(tmp.path(), &tmp.path().join("metadata"), config, mine.id()).unwrap();
        let entry = |n: usize| {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(n as u64).to_le_bytes());
            EntryId(id)
        };
        let file = |n: usize, round: usize| EntryKind::File {
            content: ContentHash([(n % 251) as u8 ^ round as u8; 32]),
            size: n as u64 + round as u64,
            executable: false,
        };
        for n in 0..ENTRIES {
            state
                .meta
                .local_change(
                    mine.signing_key(),
                    entry(n),
                    &format!("photos/{:02}/IMG_{n:07}.jpg", n % 50),
                    file(n, 0),
                    1,
                )
                .unwrap();
        }
        state.save().unwrap();
        state.committer.as_ref().unwrap().flush().unwrap();

        let (mut slowest, mut bytes) = (Duration::ZERO, 0u64);
        for round in 1..=ROUNDS {
            let versions: Vec<_> = (0..BATCH)
                .map(|i| {
                    let n = (i * 7919 + round * 104_729) % ENTRIES;
                    let current = state.meta.current(&entry(n)).unwrap().to_version();
                    let mut vector = state.meta.current_vector(&entry(n));
                    vector.increment(other.id());
                    let changed = tidefell_proto::sync::EntryVersion {
                        kind: file(n, round),
                        author: other.id(),
                        version: vector,
                        ..current
                    };
                    tidefell_proto::Signed::sign(&changed, other.signing_key())
                })
                .collect();
            // Checked before the lock, as the engine does.
            let verified: Vec<_> = versions
                .into_iter()
                .map(|v| tidefell_meta::verify_remote(FolderId([3; 32]), v).unwrap())
                .collect();
            let before = written();
            // What the engine does while it holds the folder's lock.
            let start = std::time::Instant::now();
            for version in verified {
                state.meta.apply_verified(version, 1).unwrap();
            }
            state.save().unwrap();
            slowest = slowest.max(start.elapsed());
            state.committer.as_ref().unwrap().flush().unwrap();
            bytes += written().saturating_sub(before);
        }
        let per_batch = bytes / ROUNDS as u64;
        println!(
            "{ENTRIES} entries, {ROUNDS} batches of {BATCH}: lock held at most {:.1} ms, \
             {:.2} MB written per batch",
            slowest.as_secs_f64() * 1000.0,
            per_batch as f64 / 1e6
        );
        assert!(slowest < Duration::from_millis(50), "{slowest:?}");
        assert!(per_batch < 1_000_000, "{per_batch}");
    }

    use tidefell_meta::Retention;
    use tidefell_proto::{FolderId, UserId};

    use super::*;
    use crate::config::ReplicaMode;

    /// A file whose base is brought up to a version made elsewhere, as a takeover does, is still
    /// known by its inode: a scan then tells it from a file of the same size and time put in
    /// its place, such as one renamed over it.
    #[test]
    fn catching_up_a_base_keeps_the_inode() {
        let tmp = tempfile::tempdir().unwrap();
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: tmp.path().join("folder"),
            mode: ReplicaMode::Full,
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        };
        let mine = tidefell_identity::DeviceKey::from_seed([5; 32]);
        let mut state =
            FolderState::open(tmp.path(), &tmp.path().join("metadata"), config, mine.id()).unwrap();
        let entry = EntryId([1; 16]);
        let content = ContentHash([7; 32]);
        state
            .meta
            .local_change(
                mine.signing_key(),
                entry,
                "a.txt",
                EntryKind::File {
                    content,
                    size: 3,
                    executable: false,
                },
                10,
            )
            .unwrap();
        state.record_local(
            "a.txt".to_owned(),
            LocalFile {
                entry,
                content,
                size: 3,
                mtime_ns: 1,
                executable: false,
                base: VersionVector::default(),
            },
        );
        state.local.set_inode("a.txt", 42);
        state.catch_up_bases();
        let local = state.local.get("a.txt").unwrap();
        assert!(local.base().dominates(&state.meta.current_vector(&entry)));
        assert_eq!(local.inode, 42);
    }

    #[test]
    fn receipt_times_are_kept_with_versions_made_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: tmp.path().join("folder"),
            mode: ReplicaMode::Full,
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        };
        let mine = tidefell_identity::DeviceKey::from_seed([5; 32]);
        let other = tidefell_identity::DeviceKey::from_seed([6; 32]);
        let mut state =
            FolderState::open(tmp.path(), &tmp.path().join("metadata"), config, mine.id()).unwrap();
        let file = |n: u8| tidefell_proto::sync::EntryKind::File {
            content: tidefell_proto::ContentHash([n; 32]),
            size: 1,
            executable: false,
        };
        // Made here: no time.
        state
            .meta
            .local_change(
                mine.signing_key(),
                tidefell_proto::EntryId([1; 16]),
                "mine",
                file(1),
                10,
            )
            .unwrap();
        // Made elsewhere: received at 2000, by this device's clock.
        let theirs = |n: u8| {
            let mut folder =
                tidefell_meta::Folder::new(FolderId([3; 32]), other.id(), 1, Retention::Days(30));
            folder
                .local_change(
                    other.signing_key(),
                    tidefell_proto::EntryId([n; 16]),
                    &format!("theirs{n}"),
                    file(n),
                    5,
                )
                .unwrap()
        };
        let verify = |v: tidefell_meta::Wire| {
            tidefell_meta::verify_remote(FolderId([3; 32]), v.plain().unwrap().clone()).unwrap()
        };
        state.meta.apply_verified(verify(theirs(2)), 2000).unwrap();
        // Received again while rebuilding: not when it first came, so no time.
        state.meta.set_record_receipts(false);
        state.meta.apply_verified(verify(theirs(3)), 3000).unwrap();
        state.meta.set_record_receipts(true);
        state.save().unwrap();
        state.committer.as_ref().unwrap().flush().unwrap();

        let reader = state.manifests.reader().unwrap();
        let sequences = [1, 2, 3];
        assert_eq!(
            reader.received(&sequences).unwrap(),
            vec![None, Some(2000), None]
        );
        assert!(reader.receipts_since().unwrap().is_some());
        // 0.19 reads a row as a signed version and stops there.
        let rows = reader.versions(&sequences).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].verify().unwrap().author, other.id());
    }

    #[test]
    fn loads_version_1_state() {
        let tmp = tempfile::tempdir().unwrap();
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: tmp.path().join("folder"),
            mode: ReplicaMode::Full,
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        };
        let dir = tmp.path().join("folders").join(config.id.to_string());
        fs::create_dir_all(&dir).unwrap();
        let local = LocalFileV3 {
            entry: EntryId([5; 16]),
            content: ContentHash([6; 32]),
            size: 12,
            mtime_ns: 34,
            executable: false,
        };
        let v1 = PersistedV1 {
            manifests: Vec::new(),
            local: vec![("a.txt".into(), local)],
            progress: vec![(DeviceId([7; 32]), 1, 2)],
        };
        let mut bytes = STATE_HEADER_V1.to_vec();
        bytes.extend(postcard::to_stdvec(&v1).unwrap());
        fs::write(dir.join("state"), bytes).unwrap();

        let mut state = FolderState::open(
            tmp.path(),
            &tmp.path().join("metadata"),
            config.clone(),
            DeviceId([8; 32]),
        )
        .unwrap();
        assert_eq!(
            state.local.get("a.txt").map(|l| l.to_file()),
            Some(LocalFile::from(local))
        );
        assert!(
            state
                .local
                .get("a.txt")
                .unwrap()
                .base()
                .iter()
                .next()
                .is_none(),
            "unknown base"
        );
        assert_eq!(state.progress.get(&DeviceId([7; 32])), Some(&(1, 2)));
        assert!(state.used.is_empty());

        state.save().unwrap();
        // Opening converted it to the folder's database and deleted the file (ADR 0034).
        assert!(
            !dir.join("state").exists()
                && FolderDb::exists(&db_path(&tmp.path().join("metadata"), config.id))
        );
    }
    /// A folder whose metadata cannot be saved says why, and catches up once it can (ADR 0037).
    #[test]
    fn a_failing_save_is_reported_until_it_works() {
        let tmp = tempfile::tempdir().unwrap();
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: tmp.path().join("folder"),
            mode: ReplicaMode::Full,
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        };
        let metadata = tmp.path().join("metadata");
        let mut state =
            FolderState::open(tmp.path(), &metadata, config.clone(), DeviceId([8; 32])).unwrap();
        assert_eq!(state.save_failed(), None);
        state.committer().unwrap().fail_commits(true);
        state.progress.insert(DeviceId([9; 32]), (1, 2));
        state.save().unwrap();
        let waited = std::time::Instant::now();
        while state.save_failed().is_none() {
            assert!(
                waited.elapsed() < Duration::from_secs(10),
                "the failure is reported"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(state
            .save_failed()
            .unwrap()
            .contains("a failure for a test"));
        assert!(state.committer().unwrap().flush().is_err());

        // Working again: the change is saved, and the failure is gone.
        state.committer().unwrap().fail_commits(false);
        let waited = std::time::Instant::now();
        while state.save_failed().is_some() {
            assert!(
                waited.elapsed() < Duration::from_secs(10),
                "the retry succeeds"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        state.close().unwrap();
        drop(state);
        let reopened = FolderState::open(tmp.path(), &metadata, config, DeviceId([8; 32])).unwrap();
        assert_eq!(reopened.progress.get(&DeviceId([9; 32])), Some(&(1, 2)));
    }

    /// Pins moved to the runtime settings (ADR 0034): a database that still holds them gives
    /// them to the folder once, and loses them at the next save.
    #[test]
    fn pins_move_out_of_the_database() {
        let tmp = tempfile::tempdir().unwrap();
        let metadata = tmp.path().join("metadata");
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: tmp.path().join("folder"),
            mode: ReplicaMode::Partial { budget_bytes: 1 },
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        };
        let device = DeviceId([8; 32]);
        let mut state = FolderState::open(tmp.path(), &metadata, config.clone(), device).unwrap();
        state.close().unwrap();
        drop(state);
        // As an earlier version left it.
        let path = db_path(&metadata, config.id);
        let (mut db, loaded) = FolderDb::open(&path, true).unwrap();
        let mut pins = TrackedSet::default();
        pins.insert("Photos/2026".to_owned());
        db.commit(&Changes {
            header: loaded.header,
            journal: Default::default(),
            maps: vec![("pins", pins.all())],
            durable: true,
            rebuild: None,
            activity: Vec::new(),
            at_ms: 0,
            chunks: Vec::new(),
            generation: 0,
        })
        .unwrap();
        drop(db);

        let mut state = FolderState::open(tmp.path(), &metadata, config.clone(), device).unwrap();
        assert_eq!(state.pins.iter().collect::<Vec<_>>(), ["Photos/2026"]);
        state.close().unwrap();
        drop(state);
        let (_, loaded) = FolderDb::open(&path, true).unwrap();
        assert!(loaded
            .maps
            .get("pins")
            .is_none_or(|records| records.is_empty()));
    }

    #[test]
    fn loads_version_3_state_without_bases() {
        let tmp = tempfile::tempdir().unwrap();
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: tmp.path().join("folder"),
            mode: ReplicaMode::Full,
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        };
        let dir = tmp.path().join("folders").join(config.id.to_string());
        fs::create_dir_all(&dir).unwrap();
        let local = LocalFileV3 {
            entry: EntryId([5; 16]),
            content: ContentHash([6; 32]),
            size: 12,
            mtime_ns: 34,
            executable: true,
        };
        let v3 = PersistedV3 {
            manifests: Vec::new(),
            local: vec![("a.txt".into(), local)],
            progress: Vec::new(),
            used: vec![("a.txt".into(), 5)],
            copies: vec![(ContentHash([6; 32]), DeviceId([9; 32]), 77)],
            copy_names: vec![(DeviceId([9; 32]), "nas".into())],
        };
        let mut bytes = STATE_HEADER_V3.to_vec();
        bytes.extend(postcard::to_stdvec(&v3).unwrap());
        fs::write(dir.join("state"), bytes).unwrap();

        let mut state = FolderState::open(
            tmp.path(),
            &tmp.path().join("metadata"),
            config.clone(),
            DeviceId([8; 32]),
        )
        .unwrap();
        assert_eq!(
            state.local.get("a.txt").map(|l| l.to_file()),
            Some(LocalFile::from(local))
        );
        assert_eq!(
            state.copy_names.get(&DeviceId([9; 32])).map(String::as_str),
            Some("nas")
        );

        // A base survives a save in the current format.
        let base: VersionVector = [(DeviceId([8; 32]), 3)].into_iter().collect();
        let mut file = state.local.get("a.txt").unwrap().to_file();
        file.base = base.clone();
        state.local.insert("a.txt".into(), file);
        state.save().unwrap();
        // Opening converted it to the folder's database and deleted the file (ADR 0034).
        assert!(
            !dir.join("state").exists()
                && FolderDb::exists(&db_path(&tmp.path().join("metadata"), config.id))
        );
        drop(state);
        let reopened = FolderState::open(
            tmp.path(),
            &tmp.path().join("metadata"),
            config,
            DeviceId([8; 32]),
        )
        .unwrap();
        assert_eq!(reopened.local.get("a.txt").unwrap().base(), base);
    }

    #[test]
    fn upgrading_catches_up_bases_left_behind() {
        use tidefell_identity::DeviceKey;

        let tmp = tempfile::tempdir().unwrap();
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: tmp.path().join("folder"),
            mode: ReplicaMode::Full,
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        };
        let key = DeviceKey::from_seed([5; 32]);
        let file = |n: u8| EntryKind::File {
            content: ContentHash([n; 32]),
            size: 1,
            executable: false,
        };
        let entry = EntryId([1; 16]);
        let other = EntryId([2; 16]);
        let mut state = FolderState::open(
            tmp.path(),
            &tmp.path().join("metadata"),
            config.clone(),
            key.id(),
        )
        .unwrap();
        // Written at a.txt, then renamed to b.txt. An older release left the base at the first
        // version, and its rename kept the record.
        let (_, first) = state
            .meta
            .local_change_from(
                key.signing_key(),
                entry,
                &VersionVector::new(),
                "a.txt",
                file(1),
                1,
            )
            .unwrap();
        state
            .meta
            .local_change(key.signing_key(), entry, "b.txt", file(1), 1)
            .unwrap();
        // Another file whose content on disk is older than its current version keeps its base.
        let (_, stale) = state
            .meta
            .local_change_from(
                key.signing_key(),
                other,
                &VersionVector::new(),
                "c.txt",
                file(2),
                1,
            )
            .unwrap();
        state
            .meta
            .local_change(key.signing_key(), other, "c.txt", file(3), 2)
            .unwrap();
        // As an earlier version left it: the metadata in files, and no database.
        let dir = tmp.path().join("folders").join(config.id.to_string());
        state.meta.save(&dir.join("meta")).unwrap();
        drop(state);
        fs::remove_file(db_path(&tmp.path().join("metadata"), config.id)).unwrap();
        let legacy = |entry, content: u8, base: &VersionVector| LocalFileV8 {
            entry,
            content: ContentHash([content; 32]),
            size: 1,
            mtime_ns: 1,
            executable: false,
            base: LegacyVersionVector(base.iter().map(|(d, c)| (*d, *c)).collect()),
        };
        let v8 = PersistedV8 {
            manifests: Vec::new(),
            local: vec![
                ("b.txt".into(), legacy(entry, 1, &first)),
                ("c.txt".into(), legacy(other, 2, &stale)),
            ],
            progress: Vec::new(),
            used: Vec::new(),
            copies: Vec::new(),
            copy_names: Vec::new(),
            dropped: Vec::new(),
            set_aside: Vec::new(),
            kept: Vec::new(),
            history: Vec::new(),
            pins: Vec::new(),
        };
        let mut bytes = STATE_HEADER_V8.to_vec();
        bytes.extend(postcard::to_stdvec(&v8).unwrap());
        fs::write(dir.join("state"), bytes).unwrap();

        let state =
            FolderState::open(tmp.path(), &tmp.path().join("metadata"), config, key.id()).unwrap();
        assert_eq!(
            state.local.get("b.txt").unwrap().base(),
            state.meta.current_vector(&entry)
        );
        assert_eq!(state.local.get("c.txt").unwrap().base(), stale);
    }

    #[test]
    fn loads_version_4_state_and_keeps_set_aside_files() {
        let tmp = tempfile::tempdir().unwrap();
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: tmp.path().join("folder"),
            mode: ReplicaMode::Full,
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        };
        let dir = tmp.path().join("folders").join(config.id.to_string());
        fs::create_dir_all(&dir).unwrap();
        let old = LocalFileV8 {
            entry: EntryId([5; 16]),
            content: ContentHash([6; 32]),
            size: 12,
            mtime_ns: 34,
            executable: false,
            base: LegacyVersionVector([(DeviceId([8; 32]), 2)].into_iter().collect()),
        };
        let local = LocalFile::from(old.clone());
        assert_eq!(local.base, [(DeviceId([8; 32]), 2)].into_iter().collect());
        let v4 = PersistedV4 {
            manifests: Vec::new(),
            local: vec![("a.txt".into(), old)],
            progress: Vec::new(),
            used: Vec::new(),
            copies: Vec::new(),
            copy_names: Vec::new(),
        };
        let mut bytes = STATE_HEADER_V4.to_vec();
        bytes.extend(postcard::to_stdvec(&v4).unwrap());
        fs::write(dir.join("state"), bytes).unwrap();

        let mut state = FolderState::open(
            tmp.path(),
            &tmp.path().join("metadata"),
            config.clone(),
            DeviceId([8; 32]),
        )
        .unwrap();
        assert_eq!(
            state.local.get("a.txt").map(|l| l.to_file()),
            Some(local.clone())
        );
        assert!(state.dropped.is_empty() && state.set_aside.is_empty());

        state.dropped.insert(EntryId([9; 16]));
        state.set_aside.insert(
            local.entry,
            SetAside {
                path: "a.txt".into(),
                file: "x".into(),
                local: local.clone(),
                removed_at_ms: 77,
            },
        );
        state.save().unwrap();
        // Opening converted it to the folder's database and deleted the file (ADR 0034).
        assert!(
            !dir.join("state").exists()
                && FolderDb::exists(&db_path(&tmp.path().join("metadata"), config.id))
        );
        drop(state);
        let reopened = FolderState::open(
            tmp.path(),
            &tmp.path().join("metadata"),
            config,
            DeviceId([8; 32]),
        )
        .unwrap();
        assert!(reopened.dropped.contains(&EntryId([9; 16])));
        assert_eq!(reopened.set_aside[&local.entry].removed_at_ms, 77);
    }

    #[test]
    fn loads_version_2_state_and_keeps_copies() {
        let tmp = tempfile::tempdir().unwrap();
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: tmp.path().join("folder"),
            mode: ReplicaMode::Full,
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        };
        let dir = tmp.path().join("folders").join(config.id.to_string());
        fs::create_dir_all(&dir).unwrap();
        let v2 = PersistedV2 {
            manifests: Vec::new(),
            local: Vec::new(),
            progress: Vec::new(),
            used: vec![("a.txt".into(), 99)],
        };
        let mut bytes = STATE_HEADER_V2.to_vec();
        bytes.extend(postcard::to_stdvec(&v2).unwrap());
        fs::write(dir.join("state"), bytes).unwrap();

        let mut state = FolderState::open(
            tmp.path(),
            &tmp.path().join("metadata"),
            config.clone(),
            DeviceId([8; 32]),
        )
        .unwrap();
        assert_eq!(state.used.get("a.txt"), Some(&99));
        assert!(state.copies.is_empty());

        let content = ContentHash([6; 32]);
        state
            .copies
            .entry(content)
            .or_default()
            .insert(DeviceId([9; 32]), 1234);
        state.save().unwrap();
        drop(state);
        let reopened = FolderState::open(
            tmp.path(),
            &tmp.path().join("metadata"),
            config,
            DeviceId([8; 32]),
        )
        .unwrap();
        assert_eq!(
            reopened
                .copies
                .get(&content)
                .and_then(|d| d.get(&DeviceId([9; 32]))),
            Some(&1234)
        );
    }
}
