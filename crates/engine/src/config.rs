//! Engine configuration.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tidefell_meta::Retention;
use tidefell_net::{Limits, RelayConfig, RelayUrl};
use tidefell_proto::{DeviceId, FolderId, UserId};

/// How much of a folder a replica keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaMode {
    /// Keep every file on disk and every chunk in the block store.
    Full,
    /// Keep metadata for everything; other devices' files are placeholders until opened, and
    /// the least recently used are evicted to stay within the budget (ADR 0016).
    Partial {
        /// Upper bound for the files kept on disk, in bytes.
        budget_bytes: u64,
    },
}

impl ReplicaMode {
    /// A partial replica with `budget_bytes`, or a full one for `None`.
    pub fn from_budget(budget_bytes: Option<u64>) -> Self {
        match budget_bytes {
            Some(budget_bytes) => Self::Partial { budget_bytes },
            None => Self::Full,
        }
    }

    /// The budget of a partial replica; `None` for a full one.
    pub fn budget_bytes(self) -> Option<u64> {
        match self {
            Self::Full => None,
            Self::Partial { budget_bytes } => Some(budget_bytes),
        }
    }
}

/// Whether a replica applies other devices' changes to its files (ADR 0025).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Send local changes and write other devices' changes to disk.
    #[default]
    Both,
    /// Send local changes only. Other devices' changes are recorded in the metadata but never
    /// downloaded or applied to files on this device.
    SendOnly,
}

/// What a file deleted on this device, outside the engine, means for other devices (ADR 0025).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LocalDeletes {
    /// The delete is synced: the file moves to every replica's recycle bin.
    #[default]
    Everywhere,
    /// The file is dropped from this device only. It stays on every other device and is not
    /// downloaded again, even when a new version arrives, until it is opened.
    ThisDevice,
}

/// How a folder syncs on this device. Not synced to other devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncOptions {
    /// Whether other devices' changes are applied here.
    pub direction: Direction,
    /// Whether deleting a file here deletes it everywhere.
    pub local_deletes: LocalDeletes,
}

/// One synced folder.
#[derive(Debug, Clone)]
pub struct FolderConfig {
    /// The folder's identifier, shared by all its replicas.
    pub id: FolderId,
    /// Where the folder lives on this device.
    pub path: PathBuf,
    /// Full or partial replica.
    pub mode: ReplicaMode,
    /// Users allowed to sync the folder.
    pub members: Vec<UserId>,
    /// Recycle-bin retention, chosen at first setup.
    pub retention: Retention,
    /// How often to rescan the folder for local changes.
    pub scan_interval: Duration,
    /// How the folder syncs on this device.
    pub sync: SyncOptions,
}

impl FolderConfig {
    /// True if this replica keeps every file of the folder: a full replica that applies every
    /// change and syncs its deletes. Only such a replica reports itself as full (ADR 0022) and
    /// keeps every chunk.
    pub fn holds_everything(&self) -> bool {
        self.mode == ReplicaMode::Full && self.sync == SyncOptions::default()
    }
}

/// A folder to add at runtime, before defaults: a fresh random ID when `id` is `None`, and
/// this device's user as the only member when `members` is empty. See
/// [`Engine::create_folder`](crate::Engine::create_folder).
#[derive(Debug, Clone)]
pub struct NewFolder {
    /// The folder's ID, to join a folder shared from another device; `None` for a new folder.
    pub id: Option<FolderId>,
    /// Where the folder is on disk.
    pub path: PathBuf,
    /// How much of the folder this replica keeps.
    pub mode: ReplicaMode,
    /// Users whose devices may sync the folder; empty for this device's user alone.
    pub members: Vec<UserId>,
    /// How long this replica keeps deleted files.
    pub retention: Retention,
    /// How often to scan the folder for local changes.
    pub scan_interval: Duration,
    /// How the folder syncs on this device.
    pub sync: SyncOptions,
}

/// A peer to dial: its device ID and, without address lookup, where to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddr {
    /// The peer's device ID.
    pub device: DeviceId,
    /// Known direct addresses.
    pub addrs: Vec<SocketAddr>,
    /// A relay the peer is reachable through, for when direct connections fail (a phone on a
    /// mobile network, a NAS behind NAT).
    pub relay: Option<RelayUrl>,
}

/// Everything an [`crate::Engine`] needs to start.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// State directory (block store, metadata, engine state).
    pub data_dir: PathBuf,
    /// Relay servers.
    pub relay: RelayConfig,
    /// Publish and resolve addresses through DNS/pkarr.
    pub address_lookup: bool,
    /// Announce and find known devices on the local network with mDNS (ADR 0029).
    pub local_discovery: bool,
    /// Watch folders for changes, with a full scan hourly, instead of scanning each folder on
    /// its interval (ADR 0029). Folders that cannot be watched are scanned on their interval.
    pub watch: bool,
    /// Upload and download limits for file content (ADR 0029).
    pub limits: Limits,
    /// Local socket to bind; `None` picks one.
    pub bind: Option<SocketAddr>,
    /// Peers to keep connected.
    pub peers: Vec<PeerAddr>,
    /// Folders to sync.
    pub folders: Vec<FolderConfig>,
}
