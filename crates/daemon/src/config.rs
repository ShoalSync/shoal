//! The `tidefelld` configuration file (TOML) and its JSON Schema.
//!
//! The schema is published with each release (`tidefelld config schema`) so deployments can
//! validate their configuration against the exact daemon version they run.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tidefell_engine::{EngineConfig, FolderConfig, PeerAddr, RelayConfig, ReplicaMode, Retention};
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
}

/// Full or partial replica.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Keep every file.
    #[default]
    Full,
    /// Keep metadata for everything and content within `budget_bytes`.
    Partial,
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
    /// Block store budget in bytes; required for `mode = "partial"`.
    #[serde(default)]
    pub budget_bytes: Option<u64>,
    /// User IDs (64 hex digits) allowed to sync this folder.
    pub members: Vec<String>,
    /// Days to keep deleted files in the recycle bin; 0 keeps them forever.
    pub retention_days: u32,
    /// Seconds between scans for local changes.
    #[serde(default = "default_scan_interval")]
    pub scan_interval_secs: u64,
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
                retention: match f.retention_days {
                    0 => Retention::Forever,
                    days => Retention::Days(days),
                },
                scan_interval: Duration::from_secs(f.scan_interval_secs.max(1)),
            });
        }
        Ok(EngineConfig {
            data_dir: self.data_dir.clone(),
            relay,
            address_lookup: self.network.address_lookup,
            bind: self.network.bind,
            peers,
            folders,
        })
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
    fn schema_describes_folders() {
        let schema: serde_json::Value = serde_json::from_str(&schema()).unwrap();
        assert!(schema["properties"]["folders"].is_object());
        assert!(schema["properties"]["data_dir"].is_object());
    }
}
