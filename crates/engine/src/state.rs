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

const STATE_HEADER: &[u8] = b"tidefell-engine-folder 1\n";

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
    dir: PathBuf,
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
            dir,
        };
        let state_path = state.dir.join("state");
        if state_path.exists() {
            let bytes = fs::read(&state_path).map_err(io_err(&state_path))?;
            let persisted: Persisted = bytes
                .strip_prefix(STATE_HEADER)
                .and_then(|body| postcard::from_bytes(body).ok())
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
        }
        Ok(state)
    }

    /// Writes metadata and engine state atomically.
    pub(crate) fn save(&self) -> Result<()> {
        self.meta.save(&self.dir.join("meta"))?;
        let persisted = Persisted {
            manifests: self.manifests.values().cloned().collect(),
            local: self.local.iter().map(|(p, l)| (p.clone(), *l)).collect(),
            progress: self
                .progress
                .iter()
                .map(|(d, (e, s))| (*d, *e, *s))
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
