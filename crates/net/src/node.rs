//! The device's iroh endpoint and the protocols it serves.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use iroh::endpoint::presets;
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, RelayMap, RelayMode, RelayUrl, SecretKey};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::{
    ChunkHash, ContentHash, DeviceId, FolderId, Manifest, Signed, ALPN_BLOBS_V1, ALPN_SYNC_V1,
};
use tokio::sync::oneshot;
use tracing::{debug, warn};

use crate::backend::Backend;
use crate::blobs::{self, BlobsProtocol, ChunkResult};
use crate::limits::{Limits, RateLimiter};
use crate::locks::{Locked, ReadWrite};
use crate::sync::{self, Peer};
use crate::transport::Conn;
use crate::{Error, Result};

/// The mDNS service Tidefell devices announce, apart from other iroh applications.
const MDNS_SERVICE: &str = "tidefell";

/// Which relay servers to use when direct connections fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayConfig {
    /// The public relays run by the iroh project.
    Default,
    /// Self-hosted relays, and the token they want, if they want one.
    Custom {
        urls: Vec<RelayUrl>,
        token: Option<RelayToken>,
    },
    /// No relays: only direct connections. For tests and fully local networks.
    Disabled,
}

/// A bearer token for self-hosted relays that admit only holders of it (iroh-relay's
/// `access.shared_token`). Sent to the relays in [`RelayConfig::Custom`]'s `urls`, and not shown
/// by `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct RelayToken(String);

impl RelayToken {
    /// A token, if `token` is one: not empty, and only visible ASCII, so it fits in a header.
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let token = token.into();
        (!token.is_empty() && token.bytes().all(|b| b.is_ascii_graphic())).then_some(Self(token))
    }
}

impl std::fmt::Debug for RelayToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RelayToken(..)")
    }
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
    /// Announce and find known devices' addresses on the local network with mDNS.
    pub local_discovery: bool,
    /// Upload and download limits for file content (ADR 0029).
    pub limits: Limits,
    /// Local socket address to bind; `None` picks one.
    pub bind: Option<SocketAddr>,
}

type Peers = Arc<Mutex<HashMap<DeviceId, PeerHandle>>>;

/// What happened with each device's connections since the node started (ADR 0030).
type Records = Arc<Mutex<HashMap<DeviceId, Record>>>;

#[derive(Debug, Clone, Default)]
struct Record {
    /// The name in the device's certificate, from its last session.
    name: Option<String>,
    /// When the current session started, if one is running.
    connected_since_ms: Option<i64>,
    /// When the last session ended.
    disconnected_ms: Option<i64>,
    /// Why the last attempt to connect, or the last session, failed.
    last_error: Option<String>,
    /// How far the device's clock was ahead of ours at the last handshake.
    clock_offset_ms: Option<i64>,
}

/// A relay server this node uses, and whether it is connected to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayState {
    /// The relay's URL.
    pub url: String,
    /// Connected now.
    pub connected: bool,
    /// Why the last attempt failed.
    pub error: Option<String>,
}

/// What this node knows of its network (ADR 0030), from iroh's latest network report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkReport {
    /// UDP over IPv4 works.
    pub udp_v4: bool,
    /// UDP over IPv6 works.
    pub udp_v6: bool,
    /// The NAT maps this node to a different public port for each destination, which makes
    /// direct connections through it hard.
    pub mapping_varies: Option<bool>,
    /// A public IPv4 address was found.
    pub public_v4: bool,
    /// A public IPv6 address was found.
    pub public_v6: bool,
    /// A captive portal, such as a hotel's sign-in page, blocks the network.
    pub captive_portal: Option<bool>,
}

/// This node's relays and network, for `tidefell doctor` (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connectivity {
    /// The relays in use; empty if relays are turned off.
    pub relays: Vec<RelayState>,
    /// `None` until the first report is done.
    pub report: Option<NetworkReport>,
}

/// How a connection reaches its peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    /// Directly over IP.
    Direct,
    /// Through a relay server.
    Relay,
}

/// One device's connection, as far as this node knows it (ADR 0030).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerStatus {
    /// The name in the device's certificate, if it has connected since the node started.
    pub name: Option<String>,
    /// A sync session is running.
    pub connected: bool,
    /// How the session's connection goes, if it has a path now.
    pub path: Option<PathKind>,
    /// Round-trip time on that path.
    pub rtt: Option<std::time::Duration>,
    /// When the current session started, in milliseconds since the Unix epoch.
    pub connected_since_ms: Option<i64>,
    /// When the last session ended.
    pub last_seen_ms: Option<i64>,
    /// Why the last attempt to connect, or the last session, failed. Cleared by a new session.
    pub last_error: Option<String>,
    /// How far the device's clock was ahead of this one's at the last handshake, in
    /// milliseconds, if it said (ADR 0030).
    pub clock_offset_ms: Option<i64>,
}

/// A connected sync peer.
#[derive(Debug, Clone)]
pub struct PeerHandle {
    conn: Conn,
    peer: Peer,
    limiter: Arc<RateLimiter>,
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
        self.peer.shared.read_locked().clone()
    }

    /// Requests the manifest of `content` in `folder`.
    pub async fn request_manifest(
        &self,
        folder: FolderId,
        content: ContentHash,
    ) -> Result<Manifest> {
        sync::request_manifest(&self.conn, &self.limiter, folder, content).await
    }

    /// Requests the certificate of `device`, an author in `folder` (ADR 0024).
    pub async fn request_certificate(
        &self,
        folder: FolderId,
        device: DeviceId,
    ) -> Result<Signed<DeviceCertificate>> {
        sync::request_certificate(&self.conn, &self.limiter, folder, device).await
    }
}

#[derive(Clone)]
struct SyncProtocol {
    backend: Arc<dyn Backend>,
    peers: Peers,
    records: Records,
    limiter: Arc<RateLimiter>,
}

impl std::fmt::Debug for SyncProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncProtocol").finish_non_exhaustive()
    }
}

impl ProtocolHandler for SyncProtocol {
    async fn accept(
        &self,
        conn: iroh::endpoint::Connection,
    ) -> std::result::Result<(), AcceptError> {
        self.serve(Conn::Iroh(conn)).await;
        Ok(())
    }
}

impl SyncProtocol {
    /// Runs the sync session a peer opened on `conn` until it ends.
    async fn serve(&self, conn: Conn) {
        run_session(
            conn,
            self.backend.clone(),
            self.peers.clone(),
            self.records.clone(),
            self.limiter.clone(),
            false,
            None,
        )
        .await;
    }
}

async fn run_session(
    conn: Conn,
    backend: Arc<dyn Backend>,
    peers: Peers,
    records: Records,
    limiter: Arc<RateLimiter>,
    initiator: bool,
    ready: Option<oneshot::Sender<Result<PeerHandle>>>,
) {
    let device = sync::device_of(&conn);
    let ready = Arc::new(Mutex::new(ready));
    let session_limiter = limiter.clone();
    let on_ready = {
        let conn = conn.clone();
        let peers = peers.clone();
        let records = records.clone();
        let ready = ready.clone();
        let backend = backend.clone();
        move |peer: Peer| {
            let handle = PeerHandle {
                conn,
                peer,
                limiter,
            };
            peers.locked().insert(device, handle.clone());
            let mut records = records.locked();
            let record = records.entry(device).or_default();
            record.connected_since_ms = Some(backend.now_ms());
            record.last_error = None;
            record.name = Some(handle.certificate().name.clone());
            record.clock_offset_ms = handle.peer.clock_offset_ms;
            if let Some(tx) = ready.locked().take() {
                let _ = tx.send(Ok(handle));
            }
        }
    };
    let result = sync::run(
        conn.clone(),
        backend.clone(),
        session_limiter,
        initiator,
        on_ready,
    )
    .await;
    let ended = {
        let mut peers = peers.locked();
        let current = peers
            .get(&device)
            .is_some_and(|h| h.conn.stable_id() == conn.stable_id());
        if current {
            peers.remove(&device);
        }
        current
    };
    // A session replaced by a newer one leaves the record to that one.
    if ended {
        let mut records = records.locked();
        let record = records.entry(device).or_default();
        record.connected_since_ms = None;
        record.disconnected_ms = Some(backend.now_ms());
        if let Err(e) = &result {
            record.last_error = Some(e.to_string());
        }
    }
    if let Err(e) = &result {
        debug!(%device, error = %e, "sync session ended with an error");
    }
    let pending = ready.locked().take();
    if let Some(tx) = pending {
        let _ = tx.send(Err(result.err().unwrap_or(Error::Closed)));
    }
}

/// The handlers of `sync/1` and `blobs/1`.
fn protocols(
    backend: &Arc<dyn Backend>,
    peers: &Peers,
    records: &Records,
    limiter: &Arc<RateLimiter>,
) -> (SyncProtocol, BlobsProtocol) {
    (
        SyncProtocol {
            backend: backend.clone(),
            peers: peers.clone(),
            records: records.clone(),
            limiter: limiter.clone(),
        },
        BlobsProtocol {
            backend: backend.clone(),
            limiter: limiter.clone(),
        },
    )
}

/// Binds an iroh endpoint with this device's key, serving `alpns`.
pub(crate) async fn bind_endpoint(config: &NodeConfig, alpns: Vec<Vec<u8>>) -> Result<Endpoint> {
    let relay_mode = match &config.relay {
        RelayConfig::Default => RelayMode::Default,
        RelayConfig::Disabled => RelayMode::Disabled,
        RelayConfig::Custom { urls, token } => {
            let map = RelayMap::from_iter(urls.iter().cloned());
            RelayMode::Custom(match token {
                Some(token) => map.with_auth_token(token.0.clone()),
                None => map,
            })
        }
    };
    let mut builder = Endpoint::builder(presets::N0)
        .secret_key(SecretKey::from_bytes(&config.secret_key))
        .alpns(alpns)
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
    if config.local_discovery {
        // Added after binding, so a network without multicast leaves the device syncing
        // without local discovery instead of failing to start.
        match MdnsAddressLookup::builder()
            .service_name(MDNS_SERVICE)
            .build(endpoint.id())
        {
            Ok(mdns) => {
                if let Ok(lookup) = endpoint.address_lookup() {
                    lookup.add(mdns);
                }
            }
            Err(e) => warn!(error = %e, "local discovery is unavailable"),
        }
    }
    Ok(endpoint)
}

/// How a node reaches peers.
enum Transport {
    Iroh {
        endpoint: Endpoint,
        router: Router,
    },
    /// The simulated network of ADR 0036.
    #[cfg(feature = "simulation")]
    Memory {
        network: crate::memory::Network,
        device: DeviceId,
        accepting: tokio::task::JoinHandle<()>,
    },
}

/// A running Tidefell peer endpoint.
pub struct Node {
    transport: Transport,
    backend: Arc<dyn Backend>,
    peers: Peers,
    records: Records,
    blob_conns: Mutex<HashMap<DeviceId, Conn>>,
    limiter: Arc<RateLimiter>,
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
        let endpoint =
            bind_endpoint(&config, vec![ALPN_SYNC_V1.to_vec(), ALPN_BLOBS_V1.to_vec()]).await?;

        let peers: Peers = Arc::default();
        let records: Records = Arc::default();
        let limiter = Arc::new(RateLimiter::new(config.limits));
        let (sync, blobs) = protocols(&backend, &peers, &records, &limiter);
        let router = Router::builder(endpoint.clone())
            .accept(ALPN_SYNC_V1, sync)
            .accept(ALPN_BLOBS_V1, blobs)
            .spawn();
        Ok(Self {
            transport: Transport::Iroh { endpoint, router },
            backend,
            peers,
            records,
            blob_conns: Mutex::default(),
            limiter,
        })
    }

    /// Joins the simulated `network` as `device` and starts serving `sync/1` and `blobs/1`
    /// on it (ADR 0036). It reaches only nodes on the same network.
    #[cfg(feature = "simulation")]
    pub fn spawn_simulated(
        network: &crate::memory::Network,
        device: DeviceId,
        limits: Limits,
        backend: Arc<dyn Backend>,
    ) -> Self {
        let peers: Peers = Arc::default();
        let records: Records = Arc::default();
        let limiter = Arc::new(RateLimiter::new(limits));
        let (sync, blobs) = protocols(&backend, &peers, &records, &limiter);
        let mut incoming = network.register(device);
        let accepting = tokio::spawn(async move {
            while let Some(accepted) = incoming.recv().await {
                if accepted.alpn == ALPN_SYNC_V1 {
                    let sync = sync.clone();
                    tokio::spawn(async move { sync.serve(accepted.conn).await });
                } else if accepted.alpn == ALPN_BLOBS_V1 {
                    let blobs = blobs.clone();
                    tokio::spawn(async move { blobs.serve(accepted.conn).await });
                } else {
                    accepted.conn.close(0, b"unknown protocol");
                }
            }
        });
        Self {
            transport: Transport::Memory {
                network: network.clone(),
                device,
                accepting,
            },
            backend,
            peers,
            records,
            blob_conns: Mutex::default(),
            limiter,
        }
    }

    /// Opens a connection to `addr` for protocol `alpn`.
    async fn dial(&self, addr: EndpointAddr, alpn: &[u8]) -> Result<Conn> {
        match &self.transport {
            Transport::Iroh { endpoint, .. } => endpoint
                .connect(addr, alpn)
                .await
                .map(Conn::Iroh)
                .map_err(|e| Error::Connect(e.to_string())),
            #[cfg(feature = "simulation")]
            Transport::Memory {
                network, device, ..
            } => network.dial(*device, DeviceId(*addr.id.as_bytes()), alpn),
        }
    }

    /// The upload and download limits in force.
    pub fn limits(&self) -> Limits {
        self.limiter.limits()
    }

    /// Changes the upload and download limits. Rates below [`crate::MIN_BYTES_PER_SEC`] are
    /// raised to it.
    pub fn set_limits(&self, limits: Limits) {
        self.limiter.set(limits);
    }

    /// Adds a fixed delay before every request this node sends to a peer, as a slow link's
    /// round trips would, to measure what they cost (ADR 0032). Zero unless set.
    pub fn set_request_delay(&self, delay: std::time::Duration) {
        self.limiter.set_request_delay(delay);
    }

    /// This device's ID (the endpoint's public key).
    pub fn id(&self) -> DeviceId {
        match &self.transport {
            Transport::Iroh { endpoint, .. } => DeviceId(*endpoint.id().as_bytes()),
            #[cfg(feature = "simulation")]
            Transport::Memory { device, .. } => *device,
        }
    }

    /// The addresses peers can use to reach this node.
    #[cfg_attr(
        feature = "simulation",
        expect(clippy::expect_used, reason = "a device ID is an Ed25519 public key")
    )]
    pub fn addr(&self) -> EndpointAddr {
        match &self.transport {
            Transport::Iroh { endpoint, .. } => endpoint.addr(),
            #[cfg(feature = "simulation")]
            Transport::Memory { device, .. } => EndpointAddr::new(
                iroh::EndpointId::from_bytes(device.as_bytes())
                    .expect("a device ID is a public key"),
            ),
        }
    }

    /// Dials a peer and starts a sync session. Returns once the handshake has completed.
    pub async fn connect(&self, addr: impl Into<EndpointAddr>) -> Result<PeerHandle> {
        let addr = addr.into();
        let device = DeviceId(*addr.id.as_bytes());
        let conn = match self.dial(addr, ALPN_SYNC_V1).await {
            Ok(conn) => conn,
            Err(e) => {
                self.records.locked().entry(device).or_default().last_error = Some(e.to_string());
                return Err(e);
            }
        };
        let (tx, rx) = oneshot::channel();
        tokio::spawn(run_session(
            conn,
            self.backend.clone(),
            self.peers.clone(),
            self.records.clone(),
            self.limiter.clone(),
            true,
            Some(tx),
        ));
        rx.await.map_err(|_| Error::Closed)?
    }

    /// The connection with `device` as far as this node knows it: now, if connected, and what
    /// happened since the node started.
    pub fn peer_status(&self, device: &DeviceId) -> PeerStatus {
        let record = self
            .records
            .locked()
            .get(device)
            .cloned()
            .unwrap_or_default();
        let session = self.peer(device);
        let (path, rtt) = session
            .as_ref()
            .and_then(|s| s.conn.path())
            .map(|p| {
                let kind = if p.relay {
                    PathKind::Relay
                } else {
                    PathKind::Direct
                };
                (Some(kind), Some(p.rtt))
            })
            .unwrap_or((None, None));
        PeerStatus {
            name: record.name,
            connected: session.is_some(),
            path,
            rtt,
            connected_since_ms: record.connected_since_ms.filter(|_| session.is_some()),
            last_seen_ms: if session.is_some() {
                Some(self.backend.now_ms())
            } else {
                record.disconnected_ms
            },
            last_error: record.last_error,
            clock_offset_ms: record.clock_offset_ms,
        }
    }

    /// Devices this node has connected to, or tried to, since it started.
    pub fn recorded_peers(&self) -> Vec<DeviceId> {
        self.records.locked().keys().copied().collect()
    }

    /// The relays this node uses and what it knows of its network (ADR 0030).
    pub fn connectivity(&self) -> Connectivity {
        use iroh::Watcher;
        let endpoint = match &self.transport {
            Transport::Iroh { endpoint, .. } => endpoint,
            #[cfg(feature = "simulation")]
            Transport::Memory { .. } => {
                return Connectivity {
                    relays: Vec::new(),
                    report: None,
                }
            }
        };
        let relays = endpoint
            .home_relay_status()
            .get()
            .into_iter()
            .map(|r| RelayState {
                url: r.url().to_string(),
                connected: r.is_connected(),
                error: r.last_error().map(ToString::to_string),
            })
            .collect();
        let report = endpoint.net_report().get().map(|r| NetworkReport {
            udp_v4: r.udp_v4,
            udp_v6: r.udp_v6,
            mapping_varies: r.mapping_varies_by_dest(),
            public_v4: r.global_v4.is_some(),
            public_v6: r.global_v6.is_some(),
            captive_portal: r.captive_portal,
        });
        Connectivity { relays, report }
    }

    /// File content sent and received since the node started (ADR 0030).
    pub fn transfer(&self) -> crate::Transfer {
        self.limiter.traffic.transfer()
    }

    /// File content moved since the node started, split by local network, and chunks served,
    /// for metrics (ADR 0030).
    pub fn traffic_counts(&self) -> crate::TrafficCounts {
        self.limiter.traffic.counts()
    }

    /// iroh's own metrics in the OpenMetrics text format, named `iroh_endpoint_...`, without
    /// the closing `# EOF` (ADR 0030).
    pub fn encode_iroh_metrics(&self, out: &mut String) -> std::fmt::Result {
        let endpoint = match &self.transport {
            Transport::Iroh { endpoint, .. } => endpoint,
            #[cfg(feature = "simulation")]
            Transport::Memory { .. } => return Ok(()),
        };
        let mut registry = iroh_metrics::Registry::default();
        registry
            .sub_registry_with_prefix("iroh")
            .register_all_prefixed(endpoint.metrics());
        registry.encode_openmetrics_to_writer(out)
    }

    /// Peers with an active sync session.
    pub fn peers(&self) -> Vec<PeerHandle> {
        self.peers.locked().values().cloned().collect()
    }

    /// The sync session with `device`, if connected.
    pub fn peer(&self, device: &DeviceId) -> Option<PeerHandle> {
        self.peers.locked().get(device).cloned()
    }

    async fn blob_conn(&self, addr: EndpointAddr) -> Result<Conn> {
        let id = DeviceId(*addr.id.as_bytes());
        if let Some(conn) = self.blob_conns.locked().get(&id) {
            if !conn.is_closed() {
                return Ok(conn.clone());
            }
        }
        let conn = self.dial(addr, ALPN_BLOBS_V1).await?;
        self.blob_conns.locked().insert(id, conn.clone());
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
        blobs::fetch_chunks(
            &conn,
            &self.limiter,
            self.backend.certificate(),
            folder,
            chunks,
        )
        .await
    }

    /// Asks a peer which chunks of `content` it holds: `(chunk_count, bitmap)`.
    pub async fn availability(
        &self,
        peer: impl Into<EndpointAddr>,
        folder: FolderId,
        content: ContentHash,
    ) -> Result<(u32, Vec<u8>)> {
        let conn = self.blob_conn(peer.into()).await?;
        blobs::availability(
            &conn,
            &self.limiter,
            self.backend.certificate(),
            folder,
            content,
        )
        .await
    }

    /// Asks a peer whether it is a full replica of `folder` (ADR 0022). Peers that predate
    /// the question fail with an error; treat that as a partial replica.
    pub async fn is_full_replica(
        &self,
        peer: impl Into<EndpointAddr>,
        folder: FolderId,
    ) -> Result<bool> {
        let conn = self.blob_conn(peer.into()).await?;
        blobs::replica(&conn, &self.limiter, self.backend.certificate(), folder).await
    }

    /// Starts pairing, as the approver, with the new device in `code` (ADR 0015). Returns once
    /// the new device has accepted the proof; show its confirmation code, then complete or
    /// decline the session.
    pub async fn pair(
        &self,
        code: &tidefell_proto::pair::PairingCode,
    ) -> Result<crate::pair::ApproverSession> {
        match &self.transport {
            Transport::Iroh { endpoint, .. } => {
                crate::pair::ApproverSession::start(endpoint, self.backend.certificate(), code)
                    .await
            }
            #[cfg(feature = "simulation")]
            Transport::Memory { .. } => Err(Error::Connect("pairing needs a real network".into())),
        }
    }

    /// Stops serving and closes the endpoint.
    pub async fn shutdown(self) {
        self.close().await;
    }

    /// Stops serving and closes the endpoint, closing every connection, though other tasks
    /// still hold the node: a node left serving after its engine stopped would answer peers
    /// for it, from state no longer kept.
    pub async fn close(&self) {
        match &self.transport {
            Transport::Iroh { endpoint, router } => {
                let _ = router.shutdown().await;
                endpoint.close().await;
            }
            #[cfg(feature = "simulation")]
            Transport::Memory {
                network,
                device,
                accepting,
            } => {
                accepting.abort();
                network.deregister(*device);
            }
        }
    }
}
