//! The running engine: shared state, background tasks and the public handle.
//!
//! The handle's operations are split by concern: [`folders`], [`files`], [`recycle`] and
//! [`peers`].

mod files;
mod folders;
mod peers;
mod recycle;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;

use tidefell_identity::DeviceKey;
use tidefell_net::{EndpointAddr, Node, NodeConfig};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::{DeviceId, FolderId, Signed, UserId};
use tidefell_store::BlockStore;
use tokio::sync::{broadcast, watch, Notify};
use tokio::task::{AbortHandle, JoinSet};
use tracing::{debug, info, warn};

pub use folders::FolderStatus;

use crate::backend::EngineBackend;
use crate::config::{EngineConfig, FolderConfig, PeerAddr};
use crate::runtime::Runtime;
use crate::state::FolderState;
use crate::views::Event;
use crate::{copies, evict, materialize, now_ms, scan, Error, Result};
use peers::endpoint_addr;
use recycle::purge;

/// How often to retry pending downloads and reconnect to peers.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// How often to purge expired recycle-bin items.
const PURGE_INTERVAL: Duration = Duration::from_secs(3600);
/// How often to ask full replicas about files that have no confirmed copy yet (ADR 0022).
const CONFIRM_INTERVAL: Duration = Duration::from_secs(10);

/// One folder: its state, a lock serialising disk work, and its scanner.
pub(crate) struct FolderHandle {
    state: Mutex<FolderState>,
    /// Serialises scanning and materialising, so neither sees the other's half-finished work.
    pub(crate) fs_lock: tokio::sync::Mutex<()>,
    /// True if the folder comes from the configuration file.
    configured: bool,
    scanner: Mutex<Option<AbortHandle>>,
    /// Paths opened by front-ends, with how many times; never evicted while open.
    pins: Mutex<HashMap<String, usize>>,
}

impl FolderHandle {
    pub(crate) fn lock(&self) -> MutexGuard<'_, FolderState> {
        self.state.lock().unwrap()
    }

    /// Paths currently open.
    pub(crate) fn pinned(&self) -> HashSet<String> {
        self.pins.lock().unwrap().keys().cloned().collect()
    }
}

/// State shared by the engine's tasks and its network backend.
pub(crate) struct Inner {
    pub(crate) key: DeviceKey,
    pub(crate) certificate: Signed<DeviceCertificate>,
    /// The user this device belongs to, from its certificate.
    pub(crate) user: UserId,
    pub(crate) data_dir: PathBuf,
    pub(crate) store: Mutex<BlockStore>,
    /// Contents being downloaded, whose chunks the block store keeps until they are written.
    pub(crate) fetching: Mutex<HashSet<tidefell_proto::ContentHash>>,
    folders: RwLock<BTreeMap<FolderId, Arc<FolderHandle>>>,
    configured_peers: Vec<PeerAddr>,
    runtime_peers: RwLock<Vec<PeerAddr>>,
    pub(crate) changes: watch::Sender<u64>,
    pub(crate) materialize: Notify,
    events: broadcast::Sender<Event>,
    /// Serialises changes to the folder set and their persistence.
    runtime_lock: Mutex<()>,
}

impl Inner {
    pub(crate) fn folder(&self, id: &FolderId) -> Result<Arc<FolderHandle>> {
        self.folders
            .read()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or(Error::UnknownFolder(*id))
    }

    pub(crate) fn folder_ids(&self) -> Vec<FolderId> {
        self.folders.read().unwrap().keys().copied().collect()
    }

    pub(crate) fn handles(&self) -> Vec<Arc<FolderHandle>> {
        self.folders.read().unwrap().values().cloned().collect()
    }

    /// Signals peers that the index changed and wakes the materialiser.
    pub(crate) fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
        self.materialize.notify_one();
    }

    pub(crate) fn emit(&self, event: Event) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }

    fn peers(&self) -> Vec<PeerAddr> {
        let mut peers = self.configured_peers.clone();
        for peer in self.runtime_peers.read().unwrap().iter() {
            if !peers.iter().any(|p| p.device == peer.device) {
                peers.push(peer.clone());
            }
        }
        peers
    }

    /// Writes runtime folders and peers. Callers hold `runtime_lock`.
    fn save_runtime(&self) -> Result<()> {
        let folders = self.folders.read().unwrap();
        let runtime = Runtime {
            folders: folders
                .values()
                .filter(|h| !h.configured)
                .map(|h| (&h.lock().config).into())
                .collect(),
            peers: self
                .runtime_peers
                .read()
                .unwrap()
                .iter()
                .map(Into::into)
                .collect(),
        };
        runtime.save(&self.data_dir)
    }
}

/// A running engine.
pub struct Engine {
    inner: Arc<Inner>,
    node: Arc<Node>,
    tasks: JoinSet<()>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}

fn open_folder(
    data_dir: &Path,
    config: FolderConfig,
    device: DeviceId,
    configured: bool,
) -> Result<FolderHandle> {
    std::fs::create_dir_all(&config.path).map_err(crate::io_err(&config.path))?;
    Ok(FolderHandle {
        state: Mutex::new(FolderState::open(data_dir, config, device)?),
        fs_lock: tokio::sync::Mutex::new(()),
        configured,
        scanner: Mutex::new(None),
        pins: Mutex::new(HashMap::new()),
    })
}

fn spawn_scanner(inner: &Arc<Inner>, id: FolderId, handle: &FolderHandle) {
    let interval = handle.lock().config.scan_interval;
    let task = {
        let inner = inner.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = run_scan(&inner, id).await {
                    warn!(folder = %id, error = %e, "scan failed");
                }
                tokio::time::sleep(interval).await;
            }
        })
    };
    if let Some(old) = handle.scanner.lock().unwrap().replace(task.abort_handle()) {
        old.abort();
    }
}

impl Engine {
    /// Opens state under `config.data_dir`, binds the network endpoint and starts scanning,
    /// syncing and materialising every configured and previously added folder.
    pub async fn start(
        config: EngineConfig,
        key: DeviceKey,
        certificate: Signed<DeviceCertificate>,
    ) -> Result<Self> {
        let device = key.id();
        let user = certificate
            .verify_certificate()
            .map_err(|e| Error::Invalid(format!("device certificate: {e}")))?
            .user;
        std::fs::create_dir_all(&config.data_dir).map_err(crate::io_err(&config.data_dir))?;
        let store = BlockStore::open(config.data_dir.join("blocks"))?;
        let runtime = Runtime::load(&config.data_dir)?;

        let mut folders = BTreeMap::new();
        for folder in &config.folders {
            let handle = open_folder(&config.data_dir, folder.clone(), device, true)?;
            folders.insert(folder.id, Arc::new(handle));
        }
        for stored in runtime.folders {
            let folder: FolderConfig = stored.into();
            if folders.contains_key(&folder.id) {
                continue; // The configuration file wins.
            }
            let id = folder.id;
            folders.insert(
                id,
                Arc::new(open_folder(&config.data_dir, folder, device, false)?),
            );
        }

        let inner = Arc::new(Inner {
            key: key.clone(),
            certificate,
            user,
            data_dir: config.data_dir.clone(),
            store: Mutex::new(store),
            fetching: Mutex::new(HashSet::new()),
            folders: RwLock::new(folders),
            configured_peers: config.peers.clone(),
            runtime_peers: RwLock::new(runtime.peers.into_iter().map(Into::into).collect()),
            changes: watch::channel(0).0,
            materialize: Notify::new(),
            events: broadcast::channel(256).0,
            runtime_lock: Mutex::new(()),
        });

        let node = Arc::new(
            Node::spawn(
                NodeConfig {
                    secret_key: key.seed(),
                    relay: config.relay.clone(),
                    address_lookup: config.address_lookup,
                    bind: config.bind,
                },
                Arc::new(EngineBackend(inner.clone())),
            )
            .await?,
        );
        info!(device = %device, "engine started");

        for (id, handle) in inner.folders.read().unwrap().iter() {
            spawn_scanner(&inner, *id, handle);
        }

        let mut tasks = JoinSet::new();
        {
            let (inner, node) = (inner.clone(), node.clone());
            tasks.spawn(async move {
                loop {
                    for id in inner.folder_ids() {
                        match run_materialize(&inner, &node, id).await {
                            Ok(()) | Err(Error::UnknownFolder(_)) => {}
                            Err(e) => warn!(folder = %id, error = %e, "materialise failed"),
                        }
                        match run_evict(&inner, id).await {
                            Ok(()) | Err(Error::UnknownFolder(_)) => {}
                            Err(e) => warn!(folder = %id, error = %e, "eviction failed"),
                        }
                    }
                    // Written files no longer need their chunks in the block store; the first
                    // pass also releases what earlier versions kept a second copy of (ADR 0027).
                    let work = !inner.store.lock().unwrap().is_empty()
                        || inner.handles().iter().any(|h| !h.lock().kept.is_empty());
                    if work {
                        let inner = inner.clone();
                        let _ =
                            tokio::task::spawn_blocking(move || crate::content::compact(&inner))
                                .await;
                    }
                    tokio::select! {
                        () = inner.materialize.notified() => {}
                        () = tokio::time::sleep(RETRY_INTERVAL) => {}
                    }
                }
            });
        }
        {
            let (inner, node) = (inner.clone(), node.clone());
            tasks.spawn(async move {
                loop {
                    for peer in inner.peers() {
                        if node.peer(&peer.device).is_some() {
                            continue;
                        }
                        let Some(addr) = endpoint_addr(&peer) else {
                            continue;
                        };
                        if let Err(e) = node.connect(addr).await {
                            debug!(peer = %peer.device, error = %e, "dial failed");
                        }
                    }
                    tokio::time::sleep(RETRY_INTERVAL).await;
                }
            });
        }
        {
            let (inner, node) = (inner.clone(), node.clone());
            tasks.spawn(async move {
                loop {
                    tokio::time::sleep(CONFIRM_INTERVAL).await;
                    for id in inner.folder_ids() {
                        let Ok(handle) = inner.folder(&id) else {
                            continue;
                        };
                        if let Err(e) = copies::confirm(&inner, &node, id, &handle).await {
                            warn!(folder = %id, error = %e, "confirming copies failed");
                        }
                    }
                }
            });
        }
        {
            let inner = inner.clone();
            tasks.spawn(async move {
                loop {
                    tokio::time::sleep(PURGE_INTERVAL).await;
                    purge(&inner, now_ms());
                }
            });
        }
        Ok(Self { inner, node, tasks })
    }

    /// This device's ID.
    pub fn id(&self) -> DeviceId {
        self.inner.key.id()
    }

    /// The user this device belongs to.
    pub fn user(&self) -> UserId {
        self.inner.user
    }

    /// This device's certificate.
    pub fn certificate(&self) -> &Signed<DeviceCertificate> {
        &self.inner.certificate
    }

    /// Addresses peers can use to reach this engine.
    pub fn addr(&self) -> EndpointAddr {
        self.node.addr()
    }

    /// Subscribes to change events.
    pub fn events(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// Connects to a peer now, instead of waiting for the dialer.
    pub async fn connect(&self, addr: impl Into<EndpointAddr>) -> Result<()> {
        self.node.connect(addr).await?;
        Ok(())
    }

    /// Scans `folder` for local changes now.
    pub async fn scan(&self, folder: FolderId) -> Result<()> {
        run_scan(&self.inner, folder).await
    }

    /// Materialises remote changes to `folder` now.
    pub async fn materialize(&self, folder: FolderId) -> Result<()> {
        run_materialize(&self.inner, &self.node, folder).await
    }

    /// Stops all tasks, saves state and closes the endpoint.
    pub async fn shutdown(mut self) {
        self.tasks.shutdown().await;
        let folders: Vec<(FolderId, Arc<FolderHandle>)> = self
            .inner
            .folders
            .read()
            .unwrap()
            .iter()
            .map(|(id, h)| (*id, h.clone()))
            .collect();
        for (id, handle) in folders {
            if let Some(scanner) = handle.scanner.lock().unwrap().take() {
                scanner.abort();
            }
            if let Err(e) = handle.lock().save() {
                warn!(folder = %id, error = %e, "saving state on shutdown failed");
            }
        }
        if let Ok(node) = Arc::try_unwrap(self.node) {
            node.shutdown().await;
        }
    }
}

async fn run_scan(inner: &Arc<Inner>, id: FolderId) -> Result<()> {
    let handle = inner.folder(&id)?;
    let _guard = handle.fs_lock.lock().await;
    let (inner2, handle2) = (inner.clone(), handle.clone());
    let changed = tokio::task::spawn_blocking(move || scan::scan(&inner2, &handle2))
        .await
        .expect("scan task panicked")?;
    if changed {
        inner.changes.send_modify(|n| *n += 1);
        inner.emit(Event::FolderChanged(id));
    }
    Ok(())
}

async fn run_evict(inner: &Arc<Inner>, id: FolderId) -> Result<()> {
    let handle = inner.folder(&id)?;
    let _guard = handle.fs_lock.lock().await;
    evict::evict(inner, id, &handle)
}

/// Brings `id` up to date on disk. Takes the folder's file lock only around disk work, not
/// while fetching from peers.
async fn run_materialize(inner: &Arc<Inner>, node: &Arc<Node>, id: FolderId) -> Result<()> {
    let handle = inner.folder(&id)?;
    materialize::materialize(inner, node, id, &handle).await
}
