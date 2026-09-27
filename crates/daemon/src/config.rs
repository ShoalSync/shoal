//! The `tidefelld` configuration file (TOML) and its JSON Schema.
//!
//! The schema is published with each release (`tidefelld config schema`) so deployments can
//! validate their configuration against the exact daemon version they run.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tidefell_engine::{
    EngineConfig, FolderConfig, PeerAddr, RelayConfig, ReplicaMode, Retention, SyncOptions,
};
use tidefell_proto::{DeviceId, FolderId, UserId};

/// Configuration for `tidefelld`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// State directory: device key, certificate, block store and folder metadata.
    pub data_dir: PathBuf,
    /// Network settings.
    #[serde(default)]
    pub network: Network,
    /// Folders to sync.
    #[serde(default)]
    pub folders: Vec<Folder>,
    /// Watch folders for changes and scan only where something changed, with a full scan every
    /// hour. Folders that cannot be watched, such as on a network filesystem, are scanned every
    /// `scan_interval_secs` instead (ADR 0029).
    #[serde(default = "default_true")]
    pub watch: bool,
    /// Serve the web peer. Omit to disable it.
    #[serde(default)]
    pub web: Option<Web>,
}

/// The web peer (ADR 0010, 0011).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Web {
    /// TCP address to serve the web UI on, e.g. "127.0.0.1:8080". Put a TLS reverse proxy in
    /// front of it.
    pub bind: SocketAddr,
    /// Mark session cookies Secure (HTTPS only). Turn off only for local testing without TLS.
    #[serde(default = "default_true")]
    pub secure_cookies: bool,
    /// Let signed-in accounts create links that share a file or directory with people who
    /// have no account (ADR 0019).
    #[serde(default)]
    pub share_links: bool,
}

/// Network settings.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Relay servers used when a direct connection is not possible.
    #[serde(default)]
    pub relay: Relay,
    /// Publish this device's addresses and resolve peers through DNS/pkarr.
    #[serde(default = "default_true")]
    pub address_lookup: bool,
    /// Announce this device and find known devices on the local network with mDNS, so they
    /// connect directly without internet access (ADR 0029).
    #[serde(default = "default_true")]
    pub local_discovery: bool,
    /// Local UDP socket to bind, e.g. "0.0.0.0:7777". Chosen automatically if unset.
    #[serde(default)]
    pub bind: Option<SocketAddr>,
    /// Devices to keep connected.
    #[serde(default)]
    pub peers: Vec<Peer>,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            relay: Relay::default(),
            address_lookup: true,
            local_discovery: true,
            bind: None,
            peers: Vec::new(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// Relay selection.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Relay {
    /// `default`: the public iroh relays. `custom`: the relays in `urls`. `disabled`: none.
    #[serde(default)]
    pub mode: RelayMode,
    /// Relay URLs, for `mode = "custom"`.
    #[serde(default)]
    pub urls: Vec<String>,
}

/// Which relays to use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RelayMode {
    /// The public relays run by the iroh project.
    #[default]
    Default,
    /// Self-hosted relays listed in `urls`.
    Custom,
    /// No relays: direct connections only.
    Disabled,
}

/// A device to keep connected.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    /// Device ID (64 hex digits).
    pub device: String,
    /// Known direct addresses, e.g. "192.168.1.20:7777". Optional with address lookup.
    #[serde(default)]
    pub addrs: Vec<SocketAddr>,
    /// A relay the peer is reachable through, e.g. "https://relay.example.org".
    #[serde(default)]
    pub relay: Option<String>,
}

/// Full or partial replica.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Keep every file.
    #[default]
    Full,
    /// Keep metadata for everything; other devices' files are placeholders until fetched, and
    /// the least recently used are evicted to stay within `budget_bytes`.
    Partial,
}

/// Whether other devices' changes are written to this device (ADR 0025).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// Send local changes and write other devices' changes to disk.
    #[default]
    Both,
    /// Send local changes only; other devices' changes are never written here.
    SendOnly,
}

/// What deleting a file in the folder on this device, outside Tidefell, does (ADR 0025).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LocalDeletes {
    /// The delete is synced: the file moves to every device's recycle bin.
    #[default]
    Everywhere,
    /// The file is dropped from this device only and stays on the others.
    ThisDevice,
}

/// A synced folder.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Folder {
    /// Folder ID (64 hex digits), the same on every replica. Create one with
    /// `tidefelld new-folder-id`.
    pub id: String,
    /// Where the folder lives on this device.
    pub path: PathBuf,
    /// Replica mode.
    #[serde(default)]
    pub mode: Mode,
    /// Bytes of files a partial replica keeps on disk; required for `mode = "partial"`.
    #[serde(default)]
    pub budget_bytes: Option<u64>,
    /// User IDs (64 hex digits) allowed to sync this folder.
    pub members: Vec<String>,
    /// Days to keep deleted files in the recycle bin; 0 keeps them forever.
    pub retention_days: u32,
    /// Seconds between scans for local changes.
    #[serde(default = "default_scan_interval")]
    pub scan_interval_secs: u64,
    /// Whether other devices' changes are written here. Only a full replica with the default
    /// `direction` and `local_deletes` counts as holding everything (ADR 0022).
    #[serde(default)]
    pub direction: Direction,
    /// What deleting a file here, outside Tidefell, does.
    #[serde(default)]
    pub local_deletes: LocalDeletes,
    /// Where to mount the folder with FUSE (Linux only). Files appear there, placeholders are
    /// fetched when opened, and `path` becomes backing storage not to be edited directly.
    #[serde(default)]
    pub mount: Option<PathBuf>,
}

fn default_scan_interval() -> u64 {
    10
}

/// The JSON Schema of the configuration file.
pub fn schema() -> String {
    serde_json::to_string_pretty(&schemars::schema_for!(Config)).expect("schema serialises")
}

/// Parses a configuration file.
pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

fn hex_id<T: std::str::FromStr>(what: &str, s: &str) -> Result<T, String> {
    s.parse()
        .map_err(|_| format!("{what} {s:?} is not 64 lowercase hex digits"))
}

impl Config {
    /// Validates the configuration and converts it for the engine.
    pub fn to_engine(&self) -> Result<EngineConfig, String> {
        let relay = match self.network.relay.mode {
            RelayMode::Default => RelayConfig::Default,
            RelayMode::Disabled => RelayConfig::Disabled,
            RelayMode::Custom => {
                if self.network.relay.urls.is_empty() {
                    return Err("relay mode \"custom\" needs at least one URL".into());
                }
                RelayConfig::Custom(
                    self.network
                        .relay
                        .urls
                        .iter()
                        .map(|u| u.parse().map_err(|_| format!("invalid relay URL {u:?}")))
                        .collect::<Result<_, _>>()?,
                )
            }
        };
        let peers = self
            .network
            .peers
            .iter()
            .map(|p| {
                Ok(PeerAddr {
                    device: hex_id::<DeviceId>("peer device", &p.device)?,
                    addrs: p.addrs.clone(),
                    relay: p
                        .relay
                        .as_deref()
                        .map(|r| r.parse().map_err(|_| format!("invalid relay URL {r:?}")))
                        .transpose()?,
                })
            })
            .collect::<Result<_, String>>()?;
        let mut folders: Vec<FolderConfig> = Vec::new();
        for f in &self.folders {
            let id = hex_id::<FolderId>("folder id", &f.id)?;
            if folders.iter().any(|g| g.id == id) {
                return Err(format!("folder {} is configured twice", f.id));
            }
            if f.members.is_empty() {
                return Err(format!("folder {} has no members", f.id));
            }
            let mode = match (f.mode, f.budget_bytes) {
                (Mode::Full, _) => ReplicaMode::Full,
                (Mode::Partial, Some(budget_bytes)) => ReplicaMode::Partial { budget_bytes },
                (Mode::Partial, None) => {
                    return Err(format!("partial folder {} needs budget_bytes", f.id));
                }
            };
            folders.push(FolderConfig {
                id,
                path: f.path.clone(),
                mode,
                members: f
                    .members
                    .iter()
                    .map(|m| hex_id::<UserId>("member", m))
                    .collect::<Result<_, _>>()?,
                retention: Retention::from_days(f.retention_days),
                scan_interval: Duration::from_secs(f.scan_interval_secs.max(1)),
                sync: SyncOptions {
                    direction: match f.direction {
                        Direction::Both => tidefell_engine::Direction::Both,
                        Direction::SendOnly => tidefell_engine::Direction::SendOnly,
                    },
                    local_deletes: match f.local_deletes {
                        LocalDeletes::Everywhere => tidefell_engine::LocalDeletes::Everywhere,
                        LocalDeletes::ThisDevice => tidefell_engine::LocalDeletes::ThisDevice,
                    },
                },
            });
        }
        self.mounts()?;
        Ok(EngineConfig {
            data_dir: self.data_dir.clone(),
            relay,
            address_lookup: self.network.address_lookup,
            local_discovery: self.network.local_discovery,
            watch: self.watch,
            bind: self.network.bind,
            peers,
            folders,
        })
    }

    /// Folders to mount with FUSE, and where.
    pub fn mounts(&self) -> Result<Vec<(FolderId, PathBuf)>, String> {
        let mut mounts: Vec<(FolderId, PathBuf)> = Vec::new();
        for f in &self.folders {
            let Some(mount) = &f.mount else { continue };
            if !cfg!(target_os = "linux") {
                return Err(format!(
                    "folder {}: mounting is only supported on Linux",
                    f.id
                ));
            }
            if !mount.is_absolute() {
                return Err(format!("folder {}: mount must be an absolute path", f.id));
            }
            if mount.starts_with(&f.path) || f.path.starts_with(mount) {
                return Err(format!(
                    "folder {}: mount and path must not contain each other",
                    f.id
                ));
            }
            if mounts.iter().any(|(_, m)| m == mount) {
                return Err(format!("{} is mounted twice", mount.display()));
            }
            mounts.push((hex_id::<FolderId>("folder id", &f.id)?, mount.clone()));
        }
        Ok(mounts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../tidefelld.example.toml");

    #[test]
    fn example_is_valid() {
        let config = parse(EXAMPLE).unwrap();
        let engine = config.to_engine().unwrap();
        assert_eq!(engine.folders.len(), 1);
        assert_eq!(engine.folders[0].retention, Retention::Days(30));
        assert_eq!(engine.folders[0].sync, SyncOptions::default());
    }

    #[test]
    fn reads_sync_options() {
        let text = EXAMPLE.replace(
            "scan_interval_secs = 10\n",
            "scan_interval_secs = 10\ndirection = \"send-only\"\nlocal_deletes = \"this-device\"\n",
        );
        let engine = parse(&text).unwrap().to_engine().unwrap();
        assert_eq!(
            engine.folders[0].sync,
            SyncOptions {
                direction: tidefell_engine::Direction::SendOnly,
                local_deletes: tidefell_engine::LocalDeletes::ThisDevice,
            }
        );
        let bad = EXAMPLE.replace(
            "scan_interval_secs = 10\n",
            "scan_interval_secs = 10\ndirection = \"receive-only\"\n",
        );
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse("data_dir = \"/x\"\nunknown = 1\n").is_err());
        let bad_member = EXAMPLE.replace(&"a".repeat(64), "not-hex");
        assert!(parse(&bad_member).unwrap().to_engine().is_err());
        let partial = EXAMPLE.replace("mode = \"full\"", "mode = \"partial\"");
        assert!(parse(&partial).unwrap().to_engine().is_err());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn validates_mounts() {
        let with = |mount: &str| {
            let text = EXAMPLE.replace(
                "scan_interval_secs = 10\n",
                &format!("scan_interval_secs = 10\nmount = \"{mount}\"\n"),
            );
            parse(&text).unwrap().mounts()
        };
        let mounts = with("/home/me/Photos").unwrap();
        assert_eq!(mounts[0].1, PathBuf::from("/home/me/Photos"));
        assert!(with("relative/dir").is_err());
        assert!(with("/srv/tidefell/photos/inside").is_err());
        assert!(with("/srv/tidefell").is_err());
        assert!(parse(EXAMPLE).unwrap().mounts().unwrap().is_empty());
    }

    #[test]
    fn schema_describes_folders() {
        let schema: serde_json::Value = serde_json::from_str(&schema()).unwrap();
        assert!(schema["properties"]["folders"].is_object());
        assert!(schema["properties"]["data_dir"].is_object());
    }
}
