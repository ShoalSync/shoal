//! Engine configuration.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tidefell_meta::Retention;
use tidefell_net::{RelayConfig, RelayUrl};
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
    /// Local socket to bind; `None` picks one.
    pub bind: Option<SocketAddr>,
    /// Peers to keep connected.
    pub peers: Vec<PeerAddr>,
    /// Folders to sync.
    pub folders: Vec<FolderConfig>,
}
