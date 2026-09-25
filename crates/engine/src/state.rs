//! Per-folder engine state and its persistence.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tidefell_chunker::ChunkIndex;
use tidefell_identity::FolderMembers;
use tidefell_meta::{Folder, Recycled};
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ContentHash, DeviceId, EntryId, Manifest, VersionVector};

use crate::config::FolderConfig;
use crate::{io_err, random_bytes, Error, Result};

const STATE_HEADER: &[u8] = b"tidefell-engine-folder 4\n";
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
    pub(crate) local: BTreeMap<String, LocalFile>,
    pub(crate) progress: HashMap<DeviceId, (u64, u64)>,
    /// When each file on disk was last opened or written, in milliseconds since the Unix
    /// epoch, for evicting the least recently used files of a partial replica.
    pub(crate) used: BTreeMap<String, i64>,
    /// Confirmed copies (ADR 0022): for content this device made, the full replicas that
    /// reported holding all of it, and when, in milliseconds since the Unix epoch.
    pub(crate) copies: HashMap<ContentHash, BTreeMap<DeviceId, i64>>,
    /// Certificate names of the devices in `copies`, as they were when they confirmed.
    pub(crate) copy_names: BTreeMap<DeviceId, String>,
    dir: PathBuf,
    /// Set when the folder is removed; later saves are skipped so tasks still finishing their
    /// work cannot recreate its state directory.
    removed: bool,
}

impl FolderState {
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

        let mut state = Self {
            config,
            members,
            meta,
            manifests: HashMap::new(),
            index: ChunkIndex::new(),
            local: BTreeMap::new(),
            progress: HashMap::new(),
            used: BTreeMap::new(),
            copies: HashMap::new(),
            copy_names: BTreeMap::new(),
            dir,
            removed: false,
        };
        let state_path = state.dir.join("state");
        if state_path.exists() {
            let bytes = fs::read(&state_path).map_err(io_err(&state_path))?;
            let persisted: Persisted = if let Some(body) = bytes.strip_prefix(STATE_HEADER) {
                postcard::from_bytes(body).ok()
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
    fn referenced_contents(&self) -> std::collections::HashSet<ContentHash> {
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
        state.local.get_mut("a.txt").unwrap().base = base.clone();
        state.save().unwrap();
        assert!(fs::read(dir.join("state"))
            .unwrap()
            .starts_with(STATE_HEADER));
        let reopened = FolderState::open(tmp.path(), config, DeviceId([8; 32])).unwrap();
        assert_eq!(reopened.local["a.txt"].base, base);
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
