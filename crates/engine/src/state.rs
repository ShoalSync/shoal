//! Per-folder engine state and its persistence.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tidefell_chunker::ChunkIndex;
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
use crate::tracked::{encode, Tracked, TrackedSet};
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

/// Files on disk by path, also indexed by content so chunks can be read from them instead of
/// being kept a second time in the block store (ADR 0027).
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalFiles {
    files: BTreeMap<String, LocalFile>,
    by_content: HashMap<ContentHash, BTreeSet<String>>,
    /// Paths recorded or forgotten since last saved (ADR 0034).
    changed: BTreeSet<String>,
}

impl LocalFiles {
    /// Records the file at `path`, replacing what was recorded there.
    pub(crate) fn insert(&mut self, path: String, file: LocalFile) -> Option<LocalFile> {
        self.changed.insert(path.clone());
        self.by_content
            .entry(file.content)
            .or_default()
            .insert(path.clone());
        let old = self.files.insert(path.clone(), file);
        if let Some(old) = &old {
            self.unindex(&path, old.content);
        }
        old
    }

    /// Forgets the file at `path`.
    pub(crate) fn remove(&mut self, path: &str) -> Option<LocalFile> {
        let old = self.files.remove(path)?;
        self.changed.insert(path.to_owned());
        self.unindex(path, old.content);
        Some(old)
    }

    /// The paths recorded or forgotten since last asked, encoded for the database.
    pub(crate) fn take_changed(&mut self) -> Vec<crate::tracked::Changed> {
        std::mem::take(&mut self.changed)
            .into_iter()
            .map(|path| {
                let file = self.files.get(&path).map(encode);
                (encode(&path), file)
            })
            .collect()
    }

    /// Every file, encoded for the database.
    pub(crate) fn all(&self) -> Vec<crate::tracked::Changed> {
        self.files
            .iter()
            .map(|(p, f)| (encode(p), Some(encode(f))))
            .collect()
    }

    /// Loads what [`LocalFiles::all`] and [`LocalFiles::take_changed`] wrote.
    fn decode(records: Vec<(Vec<u8>, Vec<u8>)>) -> std::result::Result<Self, postcard::Error> {
        let mut files = LocalFiles::default();
        for (path, file) in records {
            files.insert(postcard::from_bytes(&path)?, postcard::from_bytes(&file)?);
        }
        files.changed.clear();
        Ok(files)
    }

    /// Paths of the files on disk with `content`.
    pub(crate) fn paths_with(&self, content: &ContentHash) -> impl Iterator<Item = &String> {
        self.by_content.get(content).into_iter().flatten()
    }

    fn unindex(&mut self, path: &str, content: ContentHash) {
        // Still recorded at this path with the same content: keep the index entry.
        if self.files.get(path).is_some_and(|f| f.content == content) {
            return;
        }
        if let Some(paths) = self.by_content.get_mut(&content) {
            paths.remove(path);
            if paths.is_empty() {
                self.by_content.remove(&content);
            }
        }
    }
}

impl std::ops::Deref for LocalFiles {
    type Target = BTreeMap<String, LocalFile>;

    fn deref(&self) -> &Self::Target {
        &self.files
    }
}

impl FromIterator<(String, LocalFile)> for LocalFiles {
    fn from_iter<I: IntoIterator<Item = (String, LocalFile)>>(iter: I) -> Self {
        let mut out = Self::default();
        for (path, file) in iter {
            out.insert(path, file);
        }
        out.changed.clear();
        out
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
pub(crate) struct FolderState {
    pub(crate) config: FolderConfig,
    pub(crate) members: FolderMembers,
    pub(crate) meta: Folder,
    pub(crate) manifests: Tracked<ContentHash, Manifest>,
    pub(crate) index: ChunkIndex,
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
        Self::open_or_rebuild(data_dir, metadata_dir, config, device, None)
    }

    /// Opens the folder as [`open`](Self::open) does, rebuilding it because of `rebuild` if
    /// that is given.
    pub(crate) fn open_or_rebuild(
        data_dir: &Path,
        metadata_dir: &Path,
        config: FolderConfig,
        device: DeviceId,
        rebuild: Option<String>,
    ) -> Result<Self> {
        let dir = data_dir.join("folders").join(config.id.to_string());
        let existed = dir.exists();
        fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        let path = db_path(metadata_dir, config.id);
        let members = FolderMembers::new(config.id, config.members.iter().copied());

        let ignores = Ignores::load(&config.path);
        let retention = config.retention;
        let mut state = Self {
            meta: Folder::new(config.id, device, 1, retention),
            config,
            members,
            manifests: Tracked::default(),
            index: ChunkIndex::new(),
            local: LocalFiles::default(),
            progress: Tracked::default(),
            used: Tracked::default(),
            copies: Tracked::default(),
            copy_names: Tracked::default(),
            dropped: TrackedSet::default(),
            set_aside: Tracked::default(),
            kept: Tracked::default(),
            history: Tracked::default(),
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
        };
        let has_db = FolderDb::exists(&state.db_path);
        let has_files = state.dir.join("meta").exists() || state.dir.join("state").exists();
        let loaded = match rebuild {
            Some(reason) => Err(reason),
            None if has_db => match state.load() {
                Ok(db) => Ok(db),
                Err(Error::CorruptState(_)) => Err("its metadata database was damaged".into()),
                Err(e) => return Err(e),
            },
            None if !has_files && existed => Err("its metadata database was missing".into()),
            None => {
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
        state.refresh_sidecars();
        Ok(state)
    }

    /// Creates the folder's database and writes all of the folder to it.
    fn create_db(&mut self) -> Result<FolderDb> {
        let mut db = FolderDb::create(&self.db_path)?;
        db.commit(&Changes {
            header: self.meta.header(),
            journal: self.meta.full_journal(),
            maps: self.all_maps(),
            durable: true,
            rebuild: self.rebuild.clone(),
            activity: Vec::new(),
            at_ms: 0,
        })?;
        Ok(db)
    }

    /// Starts the folder over, because of `reason` (ADR 0034): a damaged database is moved
    /// aside as `<id>.corrupt-<time>`, and a new one gets a new index epoch, so every peer sends
    /// its whole index again, and the recycle bin, history and files set aside on this device
    /// back from their sidecars once its root is there. Files on disk are matched to entries by
    /// content as versions arrive; until a peer's whole index is here, the folder is rebuilding.
    fn rebuild_from_scratch(&mut self, device: DeviceId, reason: String) -> Result<FolderDb> {
        let now = crate::now_ms();
        if self.db_path.exists() {
            let aside = self
                .db_path
                .with_file_name(format!("{}.corrupt-{now}", self.config.id));
            crate::rename_durably(&self.db_path, &aside).map_err(io_err(&self.db_path))?;
        }
        let epoch = u64::from_le_bytes(random_bytes()?).max(1);
        self.meta = Folder::new(self.config.id, device, epoch, self.config.retention);
        self.manifests = Tracked::default();
        self.index = ChunkIndex::new();
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
    fn load(&mut self) -> Result<FolderDb> {
        let (db, loaded) = FolderDb::open(&self.db_path)?;
        let corrupt = |_| Error::CorruptState(self.db_path.clone());
        let mut maps = loaded.maps;
        let mut take = |name: &str| maps.remove(name).unwrap_or_default();
        self.rebuild = loaded.rebuild;
        self.meta = Folder::from_records(loaded.header, loaded.versions, loaded.recycled)
            .map_err(|_| Error::CorruptState(self.db_path.clone()))?;
        let manifests: Tracked<ContentHash, Manifest> =
            Tracked::decode(take("manifests")).map_err(corrupt)?;
        for manifest in manifests.values() {
            self.index.insert(manifest);
        }
        self.manifests = manifests;
        self.local = LocalFiles::decode(take("local")).map_err(corrupt)?;
        self.progress = Tracked::decode(take("progress")).map_err(corrupt)?;
        self.used = Tracked::decode(take("used")).map_err(corrupt)?;
        self.copies = Tracked::decode(take("copies")).map_err(corrupt)?;
        self.copy_names = Tracked::decode(take("copy_names")).map_err(corrupt)?;
        self.dropped = TrackedSet::decode(take("dropped_entries")).map_err(corrupt)?;
        self.set_aside = Tracked::decode(take("set_aside")).map_err(corrupt)?;
        self.kept = Tracked::decode(take("kept")).map_err(corrupt)?;
        self.history = Tracked::decode(take("history")).map_err(corrupt)?;
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
            ("manifests", self.manifests.all()),
            ("local", self.local.all()),
            ("progress", self.progress.all()),
            ("used", self.used.all()),
            ("copies", self.copies.all()),
            ("copy_names", self.copy_names.all()),
            ("dropped_entries", self.dropped.all()),
            ("set_aside", self.set_aside.all()),
            ("kept", self.kept.all()),
            ("history", self.history.all()),
            ("pins", self.old_pins.all()),
        ]
    }

    /// What changed in each map since the last save, encoded.
    fn changed_maps(&mut self) -> Vec<(&'static str, Vec<crate::tracked::Changed>)> {
        vec![
            ("manifests", self.manifests.take_changed()),
            ("local", self.local.take_changed()),
            ("progress", self.progress.take_changed()),
            ("used", self.used.take_changed()),
            ("copies", self.copies.take_changed()),
            ("copy_names", self.copy_names.take_changed()),
            ("dropped_entries", self.dropped.take_changed()),
            ("set_aside", self.set_aside.take_changed()),
            ("kept", self.kept.take_changed()),
            ("history", self.history.take_changed()),
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
            .cloned()
            .unwrap_or_else(|| path.to_owned())
    }

    /// Saves the folder and closes its database, so another engine in this process can open
    /// it, as Android's does when it restarts. Later saves do nothing.
    pub(crate) fn close(&mut self) -> Result<()> {
        self.save()?;
        match self.committer.take() {
            Some(committer) => committer.close(),
            None => Ok(()),
        }
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
                matches!(v.kind, EntryKind::File { content, executable, .. }
                    if content == local.content && executable == local.executable)
            });
            let here = self
                .meta
                .disk_path(&local.entry)
                .is_some_and(|p| self.same_place(path, p));
            if current && here {
                let vector = self.meta.current_vector(&local.entry);
                if !local.base.dominates(&vector) {
                    let mut local = local.clone();
                    local.base.merge(&vector);
                    caught_up.push((path.clone(), local));
                }
            }
        }
        for (path, local) in caught_up {
            self.local.insert(path, local);
        }
    }

    /// Marks the folder removed and returns its state directory for deletion.
    pub(crate) fn mark_removed(&mut self) -> PathBuf {
        self.removed = true;
        self.committer = None;
        self.dir.clone()
    }

    /// Writes metadata and engine state atomically.
    pub(crate) fn save(&mut self) -> Result<()> {
        // A removed folder's state directory is gone, and a closed one belongs to the next
        // engine: tasks still finishing their work must not write to either.
        if self.removed || self.committer.is_none() {
            return Ok(());
        }
        // Only what changed, committed in order off the folder lock (ADR 0034). Last-use times
        // alone are not worth waiting for the disk.
        let journal = self.meta.take_journal();
        let maps = self.changed_maps();
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
            at_ms: crate::now_ms(),
        };
        if let Some(committer) = &self.committer {
            committer.submit(changes);
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
        let records = self.meta.all_records();
        assert_eq!(loaded.header, self.meta.header(), "the header differs");
        let saved: Vec<(u64, Vec<u8>)> = loaded
            .versions
            .iter()
            .map(|(s, v)| (*s, v.body.clone()))
            .collect();
        let in_memory: Vec<(u64, Vec<u8>)> = records
            .stored
            .iter()
            .map(|(s, v)| (*s, v.body.clone()))
            .collect();
        assert_eq!(saved, in_memory, "the stored versions differ from memory");
        let mut saved: Vec<String> = loaded.recycled.iter().map(|r| format!("{r:?}")).collect();
        let mut in_memory: Vec<String> = records
            .recycle
            .iter()
            .filter_map(|(_, r)| r.as_ref().map(|r| format!("{r:?}")))
            .collect();
        saved.sort();
        in_memory.sort();
        assert_eq!(saved, in_memory, "the recycle bin differs from memory");
    }

    /// Replaces the member list.
    pub(crate) fn set_members(&mut self, members: Vec<tidefell_proto::UserId>) {
        self.members = FolderMembers::new(self.config.id, members.iter().copied());
        self.config.members = members;
    }

    pub(crate) fn add_manifest(&mut self, manifest: Manifest) {
        self.index.insert(&manifest);
        self.manifests.insert(manifest.content, manifest);
    }

    /// Forgets a manifest that turned out to be wrong, so it is fetched again.
    pub(crate) fn remove_manifest(&mut self, content: &ContentHash) {
        if self.manifests.remove(content).is_some() {
            self.index.remove(content);
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
            add(&version.kind);
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
            .keys()
            .filter(|c| !referenced.contains(c))
            .copied()
            .collect();
        self.copies.retain(|c, _| referenced.contains(c));
        let still_named: std::collections::HashSet<DeviceId> = self
            .copies
            .values()
            .flat_map(|d| d.keys().copied())
            .collect();
        self.copy_names.retain(|d, _| still_named.contains(d));
        let unkept: Vec<ContentHash> = self
            .kept
            .keys()
            .filter(|c| !referenced.contains(c))
            .copied()
            .collect();
        for content in unkept {
            if let Some(name) = self.kept.remove(&content) {
                let file = crate::content::kept_dir(&self.config.path).join(name);
                let _ = fs::remove_file(&file);
                crate::sidecar::remove(&file);
            }
        }
        let mut orphaned = Vec::new();
        for content in unused {
            self.manifests.remove(&content);
            orphaned.extend(self.index.remove(&content));
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
                    let current = state.meta.current(&entry(n)).unwrap().clone();
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
        assert_eq!(state.local.get("a.txt"), Some(&LocalFile::from(local)));
        assert!(
            state.local["a.txt"].base.iter().next().is_none(),
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
        };
        let device = DeviceId([8; 32]);
        let mut state = FolderState::open(tmp.path(), &metadata, config.clone(), device).unwrap();
        state.close().unwrap();
        drop(state);
        // As an earlier version left it.
        let path = db_path(&metadata, config.id);
        let (mut db, loaded) = FolderDb::open(&path).unwrap();
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
        })
        .unwrap();
        drop(db);

        let mut state = FolderState::open(tmp.path(), &metadata, config.clone(), device).unwrap();
        assert_eq!(state.pins.iter().collect::<Vec<_>>(), ["Photos/2026"]);
        state.close().unwrap();
        drop(state);
        let (_, loaded) = FolderDb::open(&path).unwrap();
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
        assert_eq!(state.local.get("a.txt"), Some(&LocalFile::from(local)));
        assert_eq!(
            state.copy_names.get(&DeviceId([9; 32])).map(String::as_str),
            Some("nas")
        );

        // A base survives a save in the current format.
        let base: VersionVector = [(DeviceId([8; 32]), 3)].into_iter().collect();
        let mut file = state.local["a.txt"].clone();
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
        assert_eq!(reopened.local["a.txt"].base, base);
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
        assert_eq!(state.local["b.txt"].base, state.meta.current_vector(&entry));
        assert_eq!(state.local["c.txt"].base, stale);
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
        assert_eq!(state.local.get("a.txt"), Some(&local));
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
