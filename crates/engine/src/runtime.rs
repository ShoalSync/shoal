//! Folders and peers added while running, persisted in `<data>/runtime`.
//!
//! Folders and peers from the configuration file are not stored here; they are re-applied at
//! every start and cannot be removed at runtime.
//!
//! Format (version 3): the bytes `tidefell-engine-runtime 3\n` followed by the postcard
//! encoding of [`Runtime`]. Version 2 had no folder sync options, and version 1 no peer relays
//! either; both are still read.

use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tidefell_meta::Retention;
use tidefell_proto::{DeviceId, FolderId, UserId};

use crate::config::{Direction, FolderConfig, LocalDeletes, PeerAddr, ReplicaMode, SyncOptions};
use crate::{io_err, Error, Result};

const HEADER: &[u8] = b"tidefell-engine-runtime 3\n";
const HEADER_V2: &[u8] = b"tidefell-engine-runtime 2\n";
const HEADER_V1: &[u8] = b"tidefell-engine-runtime 1\n";

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
    /// True for [`Direction::SendOnly`].
    send_only: bool,
    /// True for [`LocalDeletes::ThisDevice`].
    local_deletes_here: bool,
}

/// [`StoredFolder`] in formats 1 and 2, which had no sync options.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct StoredFolderV2 {
    id: FolderId,
    path: PathBuf,
    partial_budget: Option<u64>,
    members: Vec<UserId>,
    retention: Retention,
    scan_interval_secs: u64,
}

impl From<StoredFolderV2> for StoredFolder {
    fn from(v2: StoredFolderV2) -> Self {
        Self {
            id: v2.id,
            path: v2.path,
            partial_budget: v2.partial_budget,
            members: v2.members,
            retention: v2.retention,
            scan_interval_secs: v2.scan_interval_secs,
            send_only: false,
            local_deletes_here: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StoredPeer {
    device: DeviceId,
    addrs: Vec<SocketAddr>,
    relay: Option<String>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct RuntimeV2 {
    folders: Vec<StoredFolderV2>,
    peers: Vec<StoredPeer>,
}

impl From<RuntimeV2> for Runtime {
    fn from(v2: RuntimeV2) -> Self {
        Self {
            folders: v2.folders.into_iter().map(Into::into).collect(),
            peers: v2.peers,
        }
    }
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct RuntimeV1 {
    folders: Vec<StoredFolderV2>,
    peers: Vec<StoredPeerV1>,
}

#[derive(Deserialize)]
#[cfg_attr(test, derive(Serialize))]
struct StoredPeerV1 {
    device: DeviceId,
    addrs: Vec<SocketAddr>,
}

impl From<RuntimeV1> for Runtime {
    fn from(v1: RuntimeV1) -> Self {
        Self {
            folders: v1.folders.into_iter().map(Into::into).collect(),
            peers: v1
                .peers
                .into_iter()
                .map(|p| StoredPeer {
                    device: p.device,
                    addrs: p.addrs,
                    relay: None,
                })
                .collect(),
        }
    }
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
            send_only: c.sync.direction == Direction::SendOnly,
            local_deletes_here: c.sync.local_deletes == LocalDeletes::ThisDevice,
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
            sync: SyncOptions {
                direction: if s.send_only {
                    Direction::SendOnly
                } else {
                    Direction::Both
                },
                local_deletes: if s.local_deletes_here {
                    LocalDeletes::ThisDevice
                } else {
                    LocalDeletes::Everywhere
                },
            },
        }
    }
}

impl From<&PeerAddr> for StoredPeer {
    fn from(p: &PeerAddr) -> Self {
        Self {
            device: p.device,
            addrs: p.addrs.clone(),
            relay: p.relay.as_ref().map(ToString::to_string),
        }
    }
}

impl From<StoredPeer> for PeerAddr {
    fn from(s: StoredPeer) -> Self {
        Self {
            device: s.device,
            addrs: s.addrs,
            // Written by this engine, so it parses; drop it rather than fail if it ever does not.
            relay: s.relay.and_then(|r| r.parse().ok()),
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
            Ok(bytes) => if let Some(body) = bytes.strip_prefix(HEADER) {
                postcard::from_bytes(body).ok()
            } else if let Some(body) = bytes.strip_prefix(HEADER_V2) {
                postcard::from_bytes::<RuntimeV2>(body).ok().map(Into::into)
            } else {
                bytes
                    .strip_prefix(HEADER_V1)
                    .and_then(|body| postcard::from_bytes::<RuntimeV1>(body).ok())
                    .map(Into::into)
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_version_1_and_writes_the_current_version() {
        let tmp = tempfile::tempdir().unwrap();
        let v1 = RuntimeV1 {
            folders: Vec::new(),
            peers: vec![StoredPeerV1 {
                device: DeviceId([4; 32]),
                addrs: vec!["192.168.1.20:7777".parse().unwrap()],
            }],
        };
        let mut bytes = HEADER_V1.to_vec();
        bytes.extend(postcard::to_stdvec(&v1).unwrap());
        fs::write(path(tmp.path()), bytes).unwrap();

        let mut runtime = Runtime::load(tmp.path()).unwrap();
        let peer: PeerAddr = runtime.peers[0].clone().into();
        assert_eq!(peer.device, DeviceId([4; 32]));
        assert_eq!(peer.relay, None);

        runtime.add_peer(PeerAddr {
            device: DeviceId([5; 32]),
            addrs: Vec::new(),
            relay: Some("https://relay.tidefell.org./".parse().unwrap()),
        });
        runtime.save(tmp.path()).unwrap();
        assert!(fs::read(path(tmp.path())).unwrap().starts_with(HEADER));
        let reloaded = Runtime::load(tmp.path()).unwrap();
        let peer: PeerAddr = reloaded.peers[1].clone().into();
        assert_eq!(
            peer.relay.unwrap().to_string(),
            "https://relay.tidefell.org./"
        );
    }

    #[test]
    fn reads_version_2_folders_with_default_sync_options() {
        let tmp = tempfile::tempdir().unwrap();
        let v2 = RuntimeV2 {
            folders: vec![StoredFolderV2 {
                id: FolderId([1; 32]),
                path: "/photos".into(),
                partial_budget: Some(10),
                members: vec![UserId([2; 32])],
                retention: Retention::Days(30),
                scan_interval_secs: 60,
            }],
            peers: Vec::new(),
        };
        let mut bytes = HEADER_V2.to_vec();
        bytes.extend(postcard::to_stdvec(&v2).unwrap());
        fs::write(path(tmp.path()), bytes).unwrap();

        let mut runtime = Runtime::load(tmp.path()).unwrap();
        let mut folder: FolderConfig = runtime.folders[0].clone().into();
        assert_eq!(folder.mode, ReplicaMode::Partial { budget_bytes: 10 });
        assert_eq!(folder.sync, SyncOptions::default());

        folder.sync = SyncOptions {
            direction: Direction::SendOnly,
            local_deletes: LocalDeletes::ThisDevice,
        };
        runtime.folders = vec![(&folder).into()];
        runtime.save(tmp.path()).unwrap();
        let reloaded: FolderConfig = Runtime::load(tmp.path()).unwrap().folders[0].clone().into();
        assert_eq!(reloaded.sync, folder.sync);
    }
}
