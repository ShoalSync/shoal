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
use tidefell_proto::{ContentHash, DeviceId, EntryId, Manifest};

use crate::config::FolderConfig;
use crate::{io_err, random_bytes, Error, Result};

const STATE_HEADER: &[u8] = b"tidefell-engine-folder 2\n";
/// Version 1 had no last-use times.
const STATE_HEADER_V1: &[u8] = b"tidefell-engine-folder 1\n";

/// What this replica last saw or wrote at a path on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LocalFile {
    pub(crate) entry: EntryId,
    pub(crate) content: ContentHash,
    pub(crate) size: u64,
    /// Modification time in nanoseconds since the Unix epoch, as reported by the filesystem.
    pub(crate) mtime_ns: i128,
    pub(crate) executable: bool,
}

#[derive(Serialize, Deserialize)]
struct Persisted {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFile)>,
    progress: Vec<(DeviceId, u64, u64)>,
    used: Vec<(String, i64)>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct PersistedV1 {
    manifests: Vec<Manifest>,
    local: Vec<(String, LocalFile)>,
    progress: Vec<(DeviceId, u64, u64)>,
}

impl From<PersistedV1> for Persisted {
    fn from(v1: PersistedV1) -> Self {
        Self {
            manifests: v1.manifests,
            local: v1.local,
            progress: v1.progress,
            used: Vec::new(),
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
            dir,
            removed: false,
        };
        let state_path = state.dir.join("state");
        if state_path.exists() {
            let bytes = fs::read(&state_path).map_err(io_err(&state_path))?;
            let persisted: Persisted = if let Some(body) = bytes.strip_prefix(STATE_HEADER) {
                postcard::from_bytes(body).ok()
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
            local: self.local.iter().map(|(p, l)| (p.clone(), *l)).collect(),
            progress: self
                .progress
                .iter()
                .map(|(d, (e, s))| (*d, *e, *s))
                .collect(),
            used: self.used.iter().map(|(p, t)| (p.clone(), *t)).collect(),
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

    /// Purges expired recycle-bin items and forgets manifests nothing references. Returns
    /// chunks no longer referenced by this folder.
    pub(crate) fn purge(&mut self, now_ms: i64) -> (Vec<Recycled>, Vec<tidefell_proto::ChunkHash>) {
        let purged = self.meta.purge_expired(now_ms);
        let referenced = self.referenced_contents();
        let unused: Vec<ContentHash> = self
            .manifests
            .keys()
            .filter(|c| !referenced.contains(c))
            .copied()
            .collect();
        let mut orphaned = Vec::new();
        for content in unused {
            self.manifests.remove(&content);
            orphaned.extend(self.index.remove(&content));
        }
        (purged, orphaned)
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
        let local = LocalFile {
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
        assert_eq!(state.local.get("a.txt"), Some(&local));
        assert_eq!(state.progress.get(&DeviceId([7; 32])), Some(&(1, 2)));
        assert!(state.used.is_empty());

        // Saving upgrades it to the current version.
        state.save().unwrap();
        assert!(fs::read(dir.join("state"))
            .unwrap()
            .starts_with(STATE_HEADER));
    }
}
