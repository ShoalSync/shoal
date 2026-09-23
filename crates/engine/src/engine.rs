//! The running engine: shared state, background tasks and the public handle.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tidefell_identity::DeviceKey;
use tidefell_net::{EndpointAddr, EndpointId, Node, NodeConfig};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ContentHash, DeviceId, FolderId, Signed};
use tidefell_store::BlockStore;
use tokio::sync::{watch, Notify};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::backend::EngineBackend;
use crate::config::{EngineConfig, PeerAddr};
use crate::state::FolderState;
use crate::{materialize, now_ms, scan, Error, Result};

/// How often to retry pending downloads and reconnect to configured peers.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// How often to purge expired recycle-bin items.
const PURGE_INTERVAL: Duration = Duration::from_secs(3600);

/// State shared by the engine's tasks and its network backend.
pub(crate) struct Inner {
    pub(crate) key: DeviceKey,
    pub(crate) certificate: Signed<DeviceCertificate>,
    pub(crate) store: Mutex<BlockStore>,
    pub(crate) folders: BTreeMap<FolderId, Mutex<FolderState>>,
    /// Serialises scanning and materialising per folder, so neither sees the other's
    /// half-finished work.
    pub(crate) fs_locks: BTreeMap<FolderId, Arc<tokio::sync::Mutex<()>>>,
    pub(crate) changes: watch::Sender<u64>,
    pub(crate) materialize: Notify,
}

impl Inner {
    pub(crate) fn folder(&self, id: &FolderId) -> Result<&Mutex<FolderState>> {
        self.folders.get(id).ok_or(Error::UnknownFolder(*id))
    }

    /// Signals peers that the index changed and wakes the materialiser.
    pub(crate) fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
        self.materialize.notify_one();
    }
}

/// A summary of one folder, for status displays and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderStatus {
    /// Live files by path, with their content hash.
    pub files: BTreeMap<String, ContentHash>,
    /// Live files not yet written to disk on this replica.
    pub pending: usize,
    /// Entries with unresolved conflict copies.
    pub conflicts: usize,
    /// Paths of entries in the recycle bin.
    pub recycled: Vec<String>,
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

fn endpoint_addr(peer: &PeerAddr) -> Option<EndpointAddr> {
    let id = EndpointId::from_bytes(peer.device.as_bytes()).ok()?;
    Some(
        peer.addrs
            .iter()
            .fold(EndpointAddr::new(id), |addr, ip| addr.with_ip_addr(*ip)),
    )
}

impl Engine {
    /// Opens state under `config.data_dir`, binds the network endpoint and starts scanning,
    /// syncing and materialising every configured folder.
    pub async fn start(
        config: EngineConfig,
        key: DeviceKey,
        certificate: Signed<DeviceCertificate>,
    ) -> Result<Self> {
        let device = key.id();
        let store = BlockStore::open(config.data_dir.join("blocks"))?;
        let mut folders = BTreeMap::new();
        let mut fs_locks = BTreeMap::new();
        for folder in &config.folders {
            std::fs::create_dir_all(&folder.path).map_err(crate::io_err(&folder.path))?;
            let state = FolderState::open(&config.data_dir, folder.clone(), device)?;
            folders.insert(folder.id, Mutex::new(state));
            fs_locks.insert(folder.id, Arc::default());
        }
        let inner = Arc::new(Inner {
            key: key.clone(),
            certificate,
            store: Mutex::new(store),
            folders,
            fs_locks,
            changes: watch::channel(0).0,
            materialize: Notify::new(),
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

        let mut tasks = JoinSet::new();
        for folder in &config.folders {
            let (inner, id, interval) = (inner.clone(), folder.id, folder.scan_interval);
            tasks.spawn(async move {
                loop {
                    if let Err(e) = run_scan(&inner, id).await {
                        warn!(folder = %id, error = %e, "scan failed");
                    }
                    tokio::time::sleep(interval).await;
                }
            });
        }
        {
            let (inner, node) = (inner.clone(), node.clone());
            tasks.spawn(async move {
                loop {
                    for id in inner.folders.keys() {
                        if let Err(e) = run_materialize(&inner, &node, *id).await {
                            warn!(folder = %id, error = %e, "materialise failed");
                        }
                    }
                    tokio::select! {
                        () = inner.materialize.notified() => {}
                        () = tokio::time::sleep(RETRY_INTERVAL) => {}
                    }
                }
            });
        }
        {
            let node = node.clone();
            let peers = config.peers.clone();
            tasks.spawn(async move {
                loop {
                    for peer in &peers {
                        if node.peer(&peer.device).is_some() {
                            continue;
                        }
                        let Some(addr) = endpoint_addr(peer) else {
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

    /// Addresses peers can use to reach this engine.
    pub fn addr(&self) -> EndpointAddr {
        self.node.addr()
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

    /// Purges recycle-bin items that expired at or before `now_ms`, and their unused chunks.
    pub fn purge(&self, now_ms: i64) {
        purge(&self.inner, now_ms);
    }

    /// A summary of `folder`.
    pub fn status(&self, folder: FolderId) -> Result<FolderStatus> {
        let state = self.inner.folder(&folder)?.lock().unwrap();
        let mut status = FolderStatus {
            files: BTreeMap::new(),
            pending: 0,
            conflicts: 0,
            recycled: state
                .meta
                .recycle_bin()
                .iter()
                .map(|r| r.version.path.clone())
                .collect(),
        };
        for version in state.meta.entries() {
            if !state.meta.conflicts(&version.entry).is_empty() {
                status.conflicts += 1;
            }
            if let EntryKind::File { content, .. } = version.kind {
                status.files.insert(version.path.clone(), content);
                let on_disk = state
                    .local
                    .get(&version.path)
                    .is_some_and(|l| l.entry == version.entry && l.content == content);
                if !on_disk {
                    status.pending += 1;
                }
            }
        }
        Ok(status)
    }

    /// Stops all tasks, saves state and closes the endpoint.
    pub async fn shutdown(mut self) {
        self.tasks.shutdown().await;
        for (id, state) in &self.inner.folders {
            if let Err(e) = state.lock().unwrap().save() {
                warn!(folder = %id, error = %e, "saving state on shutdown failed");
            }
        }
        if let Ok(node) = Arc::try_unwrap(self.node) {
            node.shutdown().await;
        }
    }
}

async fn run_scan(inner: &Arc<Inner>, id: FolderId) -> Result<()> {
    let lock = inner
        .fs_locks
        .get(&id)
        .ok_or(Error::UnknownFolder(id))?
        .clone();
    let _guard = lock.lock().await;
    let inner2 = inner.clone();
    let changed = tokio::task::spawn_blocking(move || scan::scan(&inner2, id))
        .await
        .expect("scan task panicked")?;
    if changed {
        inner.changes.send_modify(|n| *n += 1);
    }
    Ok(())
}

async fn run_materialize(inner: &Arc<Inner>, node: &Arc<Node>, id: FolderId) -> Result<()> {
    let lock = inner
        .fs_locks
        .get(&id)
        .ok_or(Error::UnknownFolder(id))?
        .clone();
    let _guard = lock.lock().await;
    materialize::materialize(inner, node, id).await
}

fn purge(inner: &Inner, now_ms: i64) {
    let mut orphaned = Vec::new();
    for (id, state) in &inner.folders {
        let mut state = state.lock().unwrap();
        let (purged, chunks) = state.purge(now_ms);
        if !purged.is_empty() || !chunks.is_empty() {
            info!(folder = %id, items = purged.len(), chunks = chunks.len(), "purged recycle bin");
            if let Err(e) = state.save() {
                warn!(folder = %id, error = %e, "saving after purge failed");
            }
        }
        orphaned.extend(chunks);
    }
    // A chunk may still be used by another folder.
    let still_used = |hash: &tidefell_proto::ChunkHash| {
        inner
            .folders
            .values()
            .any(|s| s.lock().unwrap().index.is_referenced(hash))
    };
    let mut store = inner.store.lock().unwrap();
    for hash in orphaned {
        if !still_used(&hash) {
            let _ = store.remove(&hash);
        }
    }
}
