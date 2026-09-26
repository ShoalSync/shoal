//! Peers: connected sessions, remembered addresses and pairing.

use tidefell_net::{EndpointAddr, EndpointId};
use tidefell_proto::DeviceId;

use super::Engine;
use crate::config::PeerAddr;
use crate::views::PeerInfo;
use crate::Result;

impl Engine {
    /// Peers with an active sync session.
    pub fn connected_peers(&self) -> Vec<PeerInfo> {
        let mut peers: Vec<PeerInfo> = self
            .node
            .peers()
            .into_iter()
            .map(|p| PeerInfo {
                device: p.device(),
                user: p.certificate().user,
                name: p.certificate().name.clone(),
                shared_folders: p.shared_folders().iter().copied().collect(),
            })
            .collect();
        peers.sort_by_key(|p| p.device);
        peers
    }

    /// Peers this engine keeps connected: from the configuration and added at runtime.
    pub fn known_peers(&self) -> Vec<PeerAddr> {
        self.inner.peers()
    }

    /// Remembers a peer to keep connected, across restarts.
    pub fn add_peer(&self, peer: PeerAddr) -> Result<()> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        {
            let mut peers = self.inner.runtime_peers.write().unwrap();
            peers.retain(|p| p.device != peer.device);
            peers.push(peer);
        }
        self.inner.save_runtime()
    }

    /// Forgets a peer added at runtime. Returns false if it was not one.
    pub fn remove_peer(&self, device: DeviceId) -> Result<bool> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        let removed = {
            let mut peers = self.inner.runtime_peers.write().unwrap();
            let before = peers.len();
            peers.retain(|p| p.device != device);
            peers.len() != before
        };
        if removed {
            self.inner.save_runtime()?;
        }
        Ok(removed)
    }

    /// Starts approving a new device from its pairing code (ADR 0015). Returns once the new
    /// device has accepted the proof; show the confirmation code, then approve or decline.
    pub async fn pair(
        &self,
        code: &tidefell_proto::pair::PairingCode,
    ) -> Result<crate::pairing::PendingPairing> {
        let session = self.node.pair(code).await?;
        Ok(crate::pairing::PendingPairing::new(
            session,
            self.inner.clone(),
            self.node.clone(),
        ))
    }
}

pub(super) fn endpoint_addr(peer: &PeerAddr) -> Option<EndpointAddr> {
    let id = EndpointId::from_bytes(peer.device.as_bytes()).ok()?;
    let mut addr = EndpointAddr::new(id);
    if let Some(relay) = &peer.relay {
        addr = addr.with_relay_url(relay.clone());
    }
    Some(
        peer.addrs
            .iter()
            .fold(addr, |addr, ip| addr.with_ip_addr(*ip)),
    )
}
