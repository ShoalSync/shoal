//! Pairing (ADR 0015): approving a new device from a running engine, and the new device's side
//! before it has an engine.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use shoal_identity::{DeviceKey, UserKey};
use shoal_net::{ApproverSession, EndpointAddr, Node, NodeConfig, RelayConfig};
pub use shoal_proto::pair::{FolderOffer, PairingCode};
use shoal_proto::DeviceId;
use tracing::info;

use crate::config::PeerAddr;
use crate::engine::Inner;
use crate::runtime::Runtime;
use crate::{now_ms, Error, Result};

pub use shoal_net::{Paired, PairingEvent, PairingServer};

/// How long a pairing code stays valid.
pub const CODE_TTL: Duration = Duration::from_secs(600);

/// A pairing in progress on the approver, waiting for the person to confirm the code.
pub struct PendingPairing {
    session: ApproverSession,
    inner: Arc<Inner>,
    node: Arc<Node>,
}

impl std::fmt::Debug for PendingPairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingPairing")
            .field("device", &self.device())
            .finish_non_exhaustive()
    }
}

impl PendingPairing {
    pub(crate) fn new(session: ApproverSession, inner: Arc<Inner>, node: Arc<Node>) -> Self {
        Self {
            session,
            inner,
            node,
        }
    }

    /// The new device.
    pub fn device(&self) -> DeviceId {
        self.session.device()
    }

    /// The name the new device suggests for itself.
    pub fn suggested_name(&self) -> &str {
        self.session.suggested_name()
    }

    /// Six digits the new device also shows.
    pub fn confirmation_code(&self) -> &str {
        self.session.confirmation_code()
    }

    /// Certifies the new device as `name` with `user` (which must be this device's user) and
    /// offers it every folder here that the user is a member of.
    pub async fn approve(self, user: &UserKey, name: &str) -> Result<()> {
        if user.id() != self.inner.user {
            return Err(Error::Invalid(
                "the user key does not belong to this device's user".into(),
            ));
        }
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Invalid("the device needs a name".into()));
        }
        let folders = self
            .inner
            .handles()
            .iter()
            .filter_map(|h| {
                let state = h.lock();
                state
                    .members
                    .contains(&self.inner.user)
                    .then(|| FolderOffer {
                        id: state.config.id,
                        name: state
                            .config
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| state.config.id.to_string()),
                        members: state.config.members.clone(),
                        retention_days: match state.config.retention {
                            shoal_meta::Retention::Forever => 0,
                            shoal_meta::Retention::Days(d) => d,
                        },
                        encryption: state.config.encryption.as_ref().map(|e| {
                            shoal_proto::pair::OfferedEncryption {
                                storage_peers: e.storage_peers.clone(),
                            }
                        }),
                    })
            })
            .collect();
        let device = self.device();
        let certificate = user.certify(device, name, now_ms());
        self.session
            .complete(certificate, &self.node.addr(), folders)
            .await?;
        info!(%device, name, "paired a new device");
        Ok(())
    }

    /// Tells the new device the person declined.
    pub async fn decline(self) -> Result<()> {
        Ok(self.session.decline().await?)
    }
}

/// Starts a new device's pairing-only endpoint, before it has a certificate. Show
/// [`PairingServer::code`] and wait for [`PairingEvent::Paired`]; then save the certificate,
/// remember the approver with [`remember_approver`] and start an [`crate::Engine`].
pub async fn start_pairing(
    key: &DeviceKey,
    relay: RelayConfig,
    address_lookup: bool,
    local_discovery: bool,
    bind: Option<SocketAddr>,
    name: &str,
) -> Result<PairingServer> {
    Ok(PairingServer::start(
        NodeConfig {
            secret_key: key.seed(),
            relay,
            address_lookup,
            local_discovery,
            limits: Default::default(),
            bind,
        },
        name.to_owned(),
        CODE_TTL,
    )
    .await?)
}

/// Parses a pairing code's text form.
pub fn parse_code(text: &str) -> Result<PairingCode> {
    PairingCode::from_text(text).map_err(|e| Error::Invalid(e.to_string()))
}

/// Records the approver of a pairing as a peer to keep connected, for the engine that starts in
/// `data_dir` next.
pub fn remember_approver(data_dir: &Path, paired: &Paired) -> Result<()> {
    crate::create_data_dir(data_dir)?;
    let peer = approver_peer(&paired.approver_addr, paired.approver.device);
    let mut runtime = Runtime::load(data_dir)?;
    runtime.add_peer(peer);
    runtime.save(data_dir)
}

fn approver_peer(addr: &EndpointAddr, device: DeviceId) -> PeerAddr {
    PeerAddr {
        device,
        addrs: addr.ip_addrs().copied().collect(),
        relay: addr.relay_urls().next().cloned(),
    }
}
