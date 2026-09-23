//! Folders and peers added while running, persisted in `<data>/runtime`.
//!
//! Folders and peers from the configuration file are not stored here; they are re-applied at
//! every start and cannot be removed at runtime.
//!
//! Format (version 1): the bytes `tidefell-engine-runtime 1\n` followed by the postcard
//! encoding of [`Runtime`].

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tidefell_meta::Retention;
use tidefell_proto::{DeviceId, FolderId, UserId};

use crate::config::{FolderConfig, PeerAddr, ReplicaMode};
use crate::{io_err, Error, Result};

const HEADER: &[u8] = b"tidefell-engine-runtime 1\n";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Runtime {
    pub(crate) folders: Vec<StoredFolder>,
    pub(crate) peers: Vec<StoredPeer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredFolder {
    id: FolderId,
    path: PathBuf,
    /// `None` for a full replica, otherwise the partial replica's byte budget.
    partial_budget: Option<u64>,
    members: Vec<UserId>,
    retention: Retention,
    scan_interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredPeer {
    device: DeviceId,
    addrs: Vec<SocketAddr>,
}

impl From<&FolderConfig> for StoredFolder {
    fn from(c: &FolderConfig) -> Self {
        Self {
            id: c.id,
            path: c.path.clone(),
            partial_budget: match c.mode {
                ReplicaMode::Full => None,
                ReplicaMode::Partial { budget_bytes } => Some(budget_bytes),
            },
            members: c.members.clone(),
            retention: c.retention,
            scan_interval_secs: c.scan_interval.as_secs(),
        }
    }
}

impl From<StoredFolder> for FolderConfig {
    fn from(s: StoredFolder) -> Self {
        Self {
            id: s.id,
            path: s.path,
            mode: match s.partial_budget {
                None => ReplicaMode::Full,
                Some(budget_bytes) => ReplicaMode::Partial { budget_bytes },
            },
            members: s.members,
            retention: s.retention,
            scan_interval: Duration::from_secs(s.scan_interval_secs.max(1)),
        }
    }
}

impl From<&PeerAddr> for StoredPeer {
    fn from(p: &PeerAddr) -> Self {
        Self {
            device: p.device,
            addrs: p.addrs.clone(),
        }
    }
}

impl From<StoredPeer> for PeerAddr {
    fn from(s: StoredPeer) -> Self {
        Self {
            device: s.device,
            addrs: s.addrs,
        }
    }
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("runtime")
}

impl Runtime {
    /// Adds or replaces a peer.
    pub(crate) fn add_peer(&mut self, peer: PeerAddr) {
        let stored: StoredPeer = (&peer).into();
        self.peers.retain(|p| p.device != stored.device);
        self.peers.push(stored);
    }

    pub(crate) fn load(data_dir: &Path) -> Result<Self> {
        let path = path(data_dir);
        match fs::read(&path) {
            Ok(bytes) => bytes
                .strip_prefix(HEADER)
                .and_then(|body| postcard::from_bytes(body).ok())
                .ok_or(Error::CorruptState(path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(io_err(&path)(e)),
        }
    }

    pub(crate) fn save(&self, data_dir: &Path) -> Result<()> {
        let path = path(data_dir);
        let tmp = data_dir.join("runtime.tmp");
        let mut bytes = HEADER.to_vec();
        bytes.extend(postcard::to_stdvec(self).expect("in-memory encoding cannot fail"));
        {
            let mut file = fs::File::create(&tmp).map_err(io_err(&tmp))?;
            file.write_all(&bytes).map_err(io_err(&tmp))?;
            file.sync_all().map_err(io_err(&tmp))?;
        }
        fs::rename(&tmp, &path).map_err(io_err(&path))
    }
}
