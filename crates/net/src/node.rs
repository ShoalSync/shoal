//! The device's iroh endpoint and the protocols it serves.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use iroh::endpoint::{presets, Connection};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::{
    ChunkHash, ContentHash, DeviceId, FolderId, Manifest, ALPN_BLOBS_V1, ALPN_SYNC_V1,
};
use tokio::sync::oneshot;
use tracing::debug;

use crate::backend::Backend;
use crate::blobs::{self, BlobsProtocol, ChunkResult};
use crate::sync::{self, Peer};
use crate::{Error, Result};

/// Which relay servers to use when direct connections fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayConfig {
    /// The public relays run by the iroh project.
    Default,
    /// Self-hosted relays.
    Custom(Vec<RelayUrl>),
    /// No relays: only direct connections. For tests and fully local networks.
    Disabled,
}

/// How to start a [`Node`].
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// The device key's Ed25519 seed. The endpoint ID is its public key.
    pub secret_key: [u8; 32],
    /// Relay servers.
    pub relay: RelayConfig,
    /// Publish and resolve addresses through DNS/pkarr, so peers can be dialed by ID alone.
    pub address_lookup: bool,
    /// Local socket address to bind; `None` picks one.
    pub bind: Option<SocketAddr>,
}

type Peers = Arc<Mutex<HashMap<DeviceId, PeerHandle>>>;

/// A connected sync peer.
#[derive(Debug, Clone)]
pub struct PeerHandle {
    conn: Connection,
    peer: Peer,
}

impl PeerHandle {
    /// The peer's verified certificate.
    pub fn certificate(&self) -> &DeviceCertificate {
        &self.peer.certificate
    }

    /// The peer's device.
    pub fn device(&self) -> DeviceId {
        self.peer.certificate.device
    }

    /// Folders currently shared with this peer.
    pub fn shared_folders(&self) -> BTreeSet<FolderId> {
        self.peer.shared.read().unwrap().clone()
    }

    /// Requests the manifest of `content` in `folder`.
    pub async fn request_manifest(
        &self,
        folder: FolderId,
        content: ContentHash,
    ) -> Result<Manifest> {
        sync::request_manifest(&self.conn, folder, content).await
    }
}

#[derive(Clone)]
struct SyncProtocol {
    backend: Arc<dyn Backend>,
    peers: Peers,
}

impl std::fmt::Debug for SyncProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncProtocol").finish_non_exhaustive()
    }
}

impl ProtocolHandler for SyncProtocol {
    async fn accept(&self, conn: Connection) -> std::result::Result<(), AcceptError> {
        run_session(conn, self.backend.clone(), self.peers.clone(), false, None).await;
        Ok(())
    }
}

async fn run_session(
    conn: Connection,
    backend: Arc<dyn Backend>,
    peers: Peers,
    initiator: bool,
    ready: Option<oneshot::Sender<Result<PeerHandle>>>,
) {
    let device = sync::device_of(&conn);
    let ready = Arc::new(Mutex::new(ready));
    let on_ready = {
        let conn = conn.clone();
        let peers = peers.clone();
        let ready = ready.clone();
        move |peer: Peer| {
            let handle = PeerHandle { conn, peer };
            peers.lock().unwrap().insert(device, handle.clone());
            if let Some(tx) = ready.lock().unwrap().take() {
                let _ = tx.send(Ok(handle));
            }
        }
    };
    let result = sync::run(conn.clone(), backend, initiator, on_ready).await;
    {
        let mut peers = peers.lock().unwrap();
        if peers
            .get(&device)
            .is_some_and(|h| h.conn.stable_id() == conn.stable_id())
        {
            peers.remove(&device);
        }
    }
    if let Err(e) = &result {
        debug!(%device, error = %e, "sync session ended with an error");
    }
    let pending = ready.lock().unwrap().take();
    if let Some(tx) = pending {
        let _ = tx.send(Err(result.err().unwrap_or(Error::Closed)));
    }
}

/// A running Tidefell peer endpoint.
pub struct Node {
    endpoint: Endpoint,
    router: Router,
    backend: Arc<dyn Backend>,
    peers: Peers,
    blob_conns: Mutex<HashMap<EndpointId, Connection>>,
}

impl std::fmt::Debug for Node {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

impl Node {
    /// Binds the endpoint and starts serving `sync/1` and `blobs/1`.
    pub async fn spawn(config: NodeConfig, backend: Arc<dyn Backend>) -> Result<Self> {
        let relay_mode = match config.relay {
            RelayConfig::Default => RelayMode::Default,
            RelayConfig::Disabled => RelayMode::Disabled,
            RelayConfig::Custom(urls) => RelayMode::Custom(RelayMap::from_iter(urls)),
        };
        let mut builder = Endpoint::builder(presets::N0)
            .secret_key(SecretKey::from_bytes(&config.secret_key))
            .alpns(vec![ALPN_SYNC_V1.to_vec(), ALPN_BLOBS_V1.to_vec()])
            .relay_mode(relay_mode);
        if !config.address_lookup {
            builder = builder.clear_address_lookup();
        }
        if let Some(addr) = config.bind {
            builder = builder
                .bind_addr(addr)
                .map_err(|e| Error::Bind(e.to_string()))?;
        }
        let endpoint = builder
            .bind()
            .await
            .map_err(|e| Error::Bind(e.to_string()))?;

        let peers: Peers = Arc::default();
        let router = Router::builder(endpoint.clone())
            .accept(
                ALPN_SYNC_V1,
                SyncProtocol {
                    backend: backend.clone(),
                    peers: peers.clone(),
                },
            )
            .accept(
                ALPN_BLOBS_V1,
                BlobsProtocol {
                    backend: backend.clone(),
                },
            )
            .spawn();
        Ok(Self {
            endpoint,
            router,
            backend,
            peers,
            blob_conns: Mutex::default(),
        })
    }

    /// This device's ID (the endpoint's public key).
    pub fn id(&self) -> DeviceId {
        DeviceId(*self.endpoint.id().as_bytes())
    }

    /// The addresses peers can use to reach this node.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Dials a peer and starts a sync session. Returns once the handshake has completed.
    pub async fn connect(&self, addr: impl Into<EndpointAddr>) -> Result<PeerHandle> {
        let conn = self
            .endpoint
            .connect(addr, ALPN_SYNC_V1)
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        let (tx, rx) = oneshot::channel();
        tokio::spawn(run_session(
            conn,
            self.backend.clone(),
            self.peers.clone(),
            true,
            Some(tx),
        ));
        rx.await.map_err(|_| Error::Closed)?
    }

    /// Peers with an active sync session.
    pub fn peers(&self) -> Vec<PeerHandle> {
        self.peers.lock().unwrap().values().cloned().collect()
    }

    /// The sync session with `device`, if connected.
    pub fn peer(&self, device: &DeviceId) -> Option<PeerHandle> {
        self.peers.lock().unwrap().get(device).cloned()
    }

    async fn blob_conn(&self, addr: EndpointAddr) -> Result<Connection> {
        let id = addr.id;
        if let Some(conn) = self.blob_conns.lock().unwrap().get(&id) {
            if conn.close_reason().is_none() {
                return Ok(conn.clone());
            }
        }
        let conn = self
            .endpoint
            .connect(addr, ALPN_BLOBS_V1)
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        self.blob_conns.lock().unwrap().insert(id, conn.clone());
        Ok(conn)
    }

    /// Fetches up to 64 chunks of `folder` from a peer. Returned data is hash-verified.
    pub async fn fetch_chunks(
        &self,
        peer: impl Into<EndpointAddr>,
        folder: FolderId,
        chunks: &[ChunkHash],
    ) -> Result<Vec<ChunkResult>> {
        let conn = self.blob_conn(peer.into()).await?;
        blobs::fetch_chunks(&conn, self.backend.certificate(), folder, chunks).await
    }

    /// Asks a peer which chunks of `content` it holds: `(chunk_count, bitmap)`.
    pub async fn availability(
        &self,
        peer: impl Into<EndpointAddr>,
        folder: FolderId,
        content: ContentHash,
    ) -> Result<(u32, Vec<u8>)> {
        let conn = self.blob_conn(peer.into()).await?;
        blobs::availability(&conn, self.backend.certificate(), folder, content).await
    }

    /// Stops serving and closes the endpoint.
    pub async fn shutdown(self) {
        let _ = self.router.shutdown().await;
        self.endpoint.close().await;
    }
}
