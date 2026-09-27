//! Per-folder engine state and its persistence.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tidefell_chunker::ChunkIndex;
use tidefell_identity::FolderMembers;
use tidefell_meta::{Folder, Recycled};
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ContentHash, DeviceId, EntryId, FolderId, Manifest, VersionVector};

use crate::config::FolderConfig;
use crate::history::Previous;
use crate::ignores::Ignores;
use crate::{io_err, random_bytes, Error, Result};

const STATE_HEADER: &[u8] = b"tidefell-engine-folder 8\n";
/// Format 7, before pins (ADR 0029).
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
}

impl LocalFiles {
    /// Records the file at `path`, replacing what was recorded there.
    pub(crate) fn insert(&mut self, path: String, file: LocalFile) -> Option<LocalFile> {
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
        self.unindex(path, old.content);
        Some(old)
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
struct PersistedV7 {
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
}

impl From<PersistedV7> for Persisted {
    fn from(v7: PersistedV7) -> Self {
        Self {
            manifests: v7.manifests,
            local: v7.local,
            progress: v7.progress,
            used: v7.used,
            copies: v7.copies,
            copy_names: v7.copy_names,
            dropped: v7.dropped,
            set_aside: v7.set_aside,
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
    local: Vec<(String, LocalFile)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
    copies: Vec<(ContentHash, DeviceId, i64)>,
    copy_names: Vec<(DeviceId, String)>,
    dropped: Vec<EntryId>,
    set_aside: Vec<(EntryId, SetAside)>,
    kept: Vec<(ContentHash, String)>,
}

impl From<PersistedV6> for Persisted {
    fn from(v6: PersistedV6) -> Self {
        Self {
            manifests: v6.manifests,
            local: v6.local,
            progress: v6.progress,
            used: v6.used,
            copies: v6.copies,
            copy_names: v6.copy_names,
            dropped: v6.dropped,
            set_aside: v6.set_aside,
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
    local: Vec<(String, LocalFile)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
    copies: Vec<(ContentHash, DeviceId, i64)>,
    copy_names: Vec<(DeviceId, String)>,
    dropped: Vec<EntryId>,
    set_aside: Vec<(EntryId, SetAside)>,
}

impl From<PersistedV5> for Persisted {
    fn from(v5: PersistedV5) -> Self {
        Self {
            manifests: v5.manifests,
            local: v5.local,
            progress: v5.progress,
            used: v5.used,
            copies: v5.copies,
            copy_names: v5.copy_names,
            dropped: v5.dropped,
            set_aside: v5.set_aside,
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
    local: Vec<(String, LocalFile)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
    copies: Vec<(ContentHash, DeviceId, i64)>,
    copy_names: Vec<(DeviceId, String)>,
}

impl From<PersistedV4> for Persisted {
    fn from(v4: PersistedV4) -> Self {
        Self {
            manifests: v4.manifests,
            local: v4.local,
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
    pub(crate) manifests: HashMap<ContentHash, Manifest>,
    pub(crate) index: ChunkIndex,
    pub(crate) local: LocalFiles,
    pub(crate) progress: HashMap<DeviceId, (u64, u64)>,
    /// When each file on disk was last opened or written, in milliseconds since the Unix
    /// epoch, for evicting the least recently used files of a partial replica.
    pub(crate) used: BTreeMap<String, i64>,
    /// Confirmed copies (ADR 0022): for content this device made, the full replicas that
    /// reported holding all of it, and when, in milliseconds since the Unix epoch.
    pub(crate) copies: HashMap<ContentHash, BTreeMap<DeviceId, i64>>,
    /// Certificate names of the devices in `copies`, as they were when they confirmed.
    pub(crate) copy_names: BTreeMap<DeviceId, String>,
    /// Live entries dropped from this device only (ADR 0025): not written to disk again, even
    /// for new versions, until opened.
    pub(crate) dropped: BTreeSet<EntryId>,
    /// Files removed from this device only and kept for the recycle bin (ADR 0025).
    pub(crate) set_aside: BTreeMap<EntryId, SetAside>,
    /// Content deleted by other devices whose file was moved into the folder's reserved
    /// directory instead of being copied into the block store, by content: the file name there
    /// (ADR 0027). Removed once nothing references the content.
    pub(crate) kept: BTreeMap<ContentHash, String>,
    /// Previous versions of entries that the engine replaced on this device (ADR 0029), newest
    /// first after each purge.
    pub(crate) history: BTreeMap<EntryId, Vec<Previous>>,
    /// Paths this device keeps on disk as a partial replica (ADR 0029): files, or directories
    /// and everything below them.
    pub(crate) pins: BTreeSet<String>,
    /// This device's ignore patterns (ADR 0029), read again at every scan.
    pub(crate) ignores: Ignores,
    dir: PathBuf,
    /// Set when the folder is removed; later saves are skipped so tasks still finishing their
    /// work cannot recreate its state directory.
    removed: bool,
}

impl FolderState {
    /// True if `folder` has saved state under `data_dir`, so it is not being created or joined.
    pub(crate) fn exists(data_dir: &Path, folder: FolderId) -> bool {
        let dir = data_dir.join("folders").join(folder.to_string());
        dir.join("meta").exists() || dir.join("state").exists()
    }

    /// Loads the folder's state from `data_dir`, or creates it.
    pub(crate) fn open(data_dir: &Path, config: FolderConfig, device: DeviceId) -> Result<Self> {
        let dir = data_dir.join("folders").join(config.id.to_string());
        fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        let members = FolderMembers::new(config.id, config.members.iter().copied());

        let meta_path = dir.join("meta");
        let meta = if meta_path.exists() {
            let mut meta = Folder::load(&meta_path)?;
            meta.set_retention(config.retention);
            meta
        } else {
            let epoch = u64::from_le_bytes(random_bytes()?).max(1);
            Folder::new(config.id, device, epoch, config.retention)
        };

        let ignores = Ignores::load(&config.path);
        let mut state = Self {
            config,
            members,
            meta,
            manifests: HashMap::new(),
            index: ChunkIndex::new(),
            local: LocalFiles::default(),
            progress: HashMap::new(),
            used: BTreeMap::new(),
            copies: HashMap::new(),
            copy_names: BTreeMap::new(),
            dropped: BTreeSet::new(),
            set_aside: BTreeMap::new(),
            kept: BTreeMap::new(),
            history: BTreeMap::new(),
            pins: BTreeSet::new(),
            ignores,
            dir,
            removed: false,
        };
        let state_path = state.dir.join("state");
        if state_path.exists() {
            let bytes = fs::read(&state_path).map_err(io_err(&state_path))?;
            let persisted: Persisted = if let Some(body) = bytes.strip_prefix(STATE_HEADER) {
                postcard::from_bytes(body).ok()
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
        }
        Ok(state)
    }

    /// Marks the folder removed and returns its state directory for deletion.
    pub(crate) fn mark_removed(&mut self) -> PathBuf {
        self.removed = true;
        self.dir.clone()
    }

    /// Writes metadata and engine state atomically.
    pub(crate) fn save(&self) -> Result<()> {
        if self.removed {
            return Ok(());
        }
        self.meta.save(&self.dir.join("meta"))?;
        let persisted = Persisted {
            manifests: self.manifests.values().cloned().collect(),
            local: self
                .local
                .iter()
                .map(|(p, l)| (p.clone(), l.clone()))
                .collect(),
            progress: self
                .progress
                .iter()
                .map(|(d, (e, s))| (*d, *e, *s))
                .collect(),
            used: self.used.iter().map(|(p, t)| (p.clone(), *t)).collect(),
            copies: self
                .copies
                .iter()
                .flat_map(|(c, devices)| devices.iter().map(|(d, at)| (*c, *d, *at)))
                .collect(),
            copy_names: self
                .copy_names
                .iter()
                .map(|(d, n)| (*d, n.clone()))
                .collect(),
            dropped: self.dropped.iter().copied().collect(),
            set_aside: self
                .set_aside
                .iter()
                .map(|(e, s)| (*e, s.clone()))
                .collect(),
            kept: self.kept.iter().map(|(c, f)| (*c, f.clone())).collect(),
            history: self.history.iter().map(|(e, v)| (*e, v.clone())).collect(),
            pins: self.pins.iter().cloned().collect(),
        };
        let mut bytes = STATE_HEADER.to_vec();
        bytes.extend(postcard::to_stdvec(&persisted).expect("in-memory encoding cannot fail"));
        let path = self.dir.join("state");
        let tmp = self.dir.join("state.tmp");
        {
            let mut file = fs::File::create(&tmp).map_err(io_err(&tmp))?;
            file.write_all(&bytes).map_err(io_err(&tmp))?;
            file.sync_all().map_err(io_err(&tmp))?;
        }
        fs::rename(&tmp, &path).map_err(io_err(&path))
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
                let _ = fs::remove_file(crate::content::kept_dir(&self.config.path).join(name));
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

        let state = FolderState::open(tmp.path(), config, DeviceId([8; 32])).unwrap();
        assert_eq!(state.local.get("a.txt"), Some(&LocalFile::from(local)));
        assert!(
            state.local["a.txt"].base.iter().next().is_none(),
            "unknown base"
        );
        assert_eq!(state.progress.get(&DeviceId([7; 32])), Some(&(1, 2)));
        assert!(state.used.is_empty());

        // Saving upgrades it to the current version.
        state.save().unwrap();
        assert!(fs::read(dir.join("state"))
            .unwrap()
            .starts_with(STATE_HEADER));
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

        let mut state = FolderState::open(tmp.path(), config.clone(), DeviceId([8; 32])).unwrap();
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
        assert!(fs::read(dir.join("state"))
            .unwrap()
            .starts_with(STATE_HEADER));
        let reopened = FolderState::open(tmp.path(), config, DeviceId([8; 32])).unwrap();
        assert_eq!(reopened.local["a.txt"].base, base);
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
        };
        let dir = tmp.path().join("folders").join(config.id.to_string());
        fs::create_dir_all(&dir).unwrap();
        let local = LocalFile {
            entry: EntryId([5; 16]),
            content: ContentHash([6; 32]),
            size: 12,
            mtime_ns: 34,
            executable: false,
            base: [(DeviceId([8; 32]), 2)].into_iter().collect(),
        };
        let v4 = PersistedV4 {
            manifests: Vec::new(),
            local: vec![("a.txt".into(), local.clone())],
            progress: Vec::new(),
            used: Vec::new(),
            copies: Vec::new(),
            copy_names: Vec::new(),
        };
        let mut bytes = STATE_HEADER_V4.to_vec();
        bytes.extend(postcard::to_stdvec(&v4).unwrap());
        fs::write(dir.join("state"), bytes).unwrap();

        let mut state = FolderState::open(tmp.path(), config.clone(), DeviceId([8; 32])).unwrap();
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
        assert!(fs::read(dir.join("state"))
            .unwrap()
            .starts_with(STATE_HEADER));
        let reopened = FolderState::open(tmp.path(), config, DeviceId([8; 32])).unwrap();
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

        let mut state = FolderState::open(tmp.path(), config.clone(), DeviceId([8; 32])).unwrap();
        assert_eq!(state.used.get("a.txt"), Some(&99));
        assert!(state.copies.is_empty());

        let content = ContentHash([6; 32]);
        state
            .copies
            .entry(content)
            .or_default()
            .insert(DeviceId([9; 32]), 1234);
        state.save().unwrap();
        let reopened = FolderState::open(tmp.path(), config, DeviceId([8; 32])).unwrap();
        assert_eq!(
            reopened
                .copies
                .get(&content)
                .and_then(|d| d.get(&DeviceId([9; 32]))),
            Some(&1234)
        );
    }
}
