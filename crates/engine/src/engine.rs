//! The running engine: shared state, background tasks and the public handle.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;

use tidefell_identity::DeviceKey;
use tidefell_net::{EndpointAddr, EndpointId, Node, NodeConfig};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ContentHash, DeviceId, EntryId, FolderId, Signed, UserId};
use tidefell_store::BlockStore;
use tokio::sync::{broadcast, watch, Notify};
use tokio::task::{AbortHandle, JoinSet};
use tracing::{debug, info, warn};

use crate::backend::EngineBackend;
use crate::config::{EngineConfig, FolderConfig, PeerAddr};
use crate::runtime::Runtime;
use crate::state::FolderState;
use crate::views::{Event, FileInfo, FolderInfo, PeerInfo, RecycledInfo};
use crate::{evict, materialize, now_ms, scan, Error, Result};

/// How often to retry pending downloads and reconnect to peers.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// How often to purge expired recycle-bin items.
const PURGE_INTERVAL: Duration = Duration::from_secs(3600);

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
    pub(crate) data_dir: PathBuf,
    pub(crate) store: Mutex<BlockStore>,
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
            data_dir: config.data_dir.clone(),
            store: Mutex::new(store),
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
                        match run_evict(&inner, &node, id).await {
                            Ok(()) | Err(Error::UnknownFolder(_)) => {}
                            Err(e) => warn!(folder = %id, error = %e, "eviction failed"),
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

    /// Purges recycle-bin items that expired at or before `now_ms`, and their unused chunks.
    pub fn purge(&self, now_ms: i64) {
        purge(&self.inner, now_ms);
    }

    // ---- Folders ----

    /// All folders.
    pub fn folders(&self) -> Vec<FolderInfo> {
        self.inner
            .folders
            .read()
            .unwrap()
            .values()
            .map(|h| {
                let state = h.lock();
                FolderInfo {
                    id: state.config.id,
                    path: state.config.path.clone(),
                    mode: state.config.mode,
                    members: state.config.members.clone(),
                    retention: state.config.retention,
                    configured: h.configured,
                }
            })
            .collect()
    }

    /// Adds a folder at runtime: a new one (with a fresh ID) or one shared from another device.
    /// It is remembered across restarts.
    pub fn add_folder(&self, config: FolderConfig) -> Result<()> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        let id = config.id;
        if self.inner.folders.read().unwrap().contains_key(&id) {
            return Err(Error::FolderExists(id));
        }
        if config.members.is_empty() {
            return Err(Error::Invalid("a folder needs at least one member".into()));
        }
        let handle = Arc::new(open_folder(&self.inner.data_dir, config, self.id(), false)?);
        self.inner
            .folders
            .write()
            .unwrap()
            .insert(id, handle.clone());
        self.inner.save_runtime()?;
        spawn_scanner(&self.inner, id, &handle);
        info!(folder = %id, "folder added");
        self.inner.emit(Event::FolderAdded(id));
        // Offer the new folder to connected peers on their next session.
        self.inner.changed();
        Ok(())
    }

    /// Stops syncing a folder added at runtime and forgets its metadata. Files on disk are
    /// left in place.
    pub fn remove_folder(&self, id: FolderId) -> Result<()> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        let handle = self.inner.folder(&id)?;
        if handle.configured {
            return Err(Error::Configured(id));
        }
        self.inner.folders.write().unwrap().remove(&id);
        if let Some(scanner) = handle.scanner.lock().unwrap().take() {
            scanner.abort();
        }
        self.inner.save_runtime()?;
        // Taking the state lock waits for any save in progress; later saves are skipped.
        let dir = handle.lock().mark_removed();
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(crate::io_err(&dir))?;
        }
        info!(folder = %id, "folder removed");
        self.inner.emit(Event::FolderRemoved(id));
        // Sessions stop offering the folder.
        self.inner.changed();
        Ok(())
    }

    fn change_members(
        &self,
        id: FolderId,
        change: impl FnOnce(&mut Vec<UserId>) -> bool,
    ) -> Result<bool> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        let handle = self.inner.folder(&id)?;
        if handle.configured {
            return Err(Error::Configured(id));
        }
        let changed = {
            let mut state = handle.lock();
            let mut members = state.config.members.clone();
            let changed = change(&mut members);
            if members.is_empty() {
                return Err(Error::Invalid("a folder needs at least one member".into()));
            }
            if changed {
                state.set_members(members);
            }
            changed
        };
        if changed {
            self.inner.save_runtime()?;
            self.inner.emit(Event::FolderChanged(id));
            // Sessions re-offer folders to peers whose access changed.
            self.inner.changed();
        }
        Ok(changed)
    }

    /// Lets `user`'s devices sync `folder`. Returns false if already a member.
    pub fn add_member(&self, folder: FolderId, user: UserId) -> Result<bool> {
        self.change_members(folder, |m| {
            if m.contains(&user) {
                false
            } else {
                m.push(user);
                true
            }
        })
    }

    /// Stops `user`'s devices syncing `folder` from now on. Returns false if not a member.
    pub fn remove_member(&self, folder: FolderId, user: UserId) -> Result<bool> {
        self.change_members(folder, |m| {
            let before = m.len();
            m.retain(|u| *u != user);
            m.len() != before
        })
    }

    // ---- Files and the recycle bin ----

    /// Live files in `folder`, sorted by path.
    pub fn files(&self, folder: FolderId) -> Result<Vec<FileInfo>> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let mut files: Vec<FileInfo> = state
            .meta
            .entries()
            .filter_map(|v| match v.kind {
                EntryKind::File {
                    content,
                    size,
                    executable,
                } => Some(FileInfo {
                    entry: v.entry,
                    path: v.path.clone(),
                    size,
                    modified_ms: v.modified_ms,
                    executable,
                    on_disk: state
                        .local
                        .get(&v.path)
                        .is_some_and(|l| l.entry == v.entry && l.content == content),
                    conflicts: state.meta.conflicts(&v.entry).len(),
                    author: v.author,
                }),
                _ => None,
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// Where a live file of `folder` is on this device's disk, if its current version is there.
    pub fn local_path(&self, folder: FolderId, path: &str) -> Result<Option<PathBuf>> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let on_disk = state.meta.find_path(path).is_some_and(|v| match v.kind {
            EntryKind::File { content, .. } => state
                .local
                .get(path)
                .is_some_and(|l| l.entry == v.entry && l.content == content),
            _ => false,
        });
        Ok(on_disk.then(|| state.config.path.join(path)))
    }

    /// Makes the live file at `path` available on disk and returns where it is, fetching it
    /// from connected peers if this replica does not have its current version (a placeholder
    /// in a partial replica, or a file still pending in a full one). The file stays pinned,
    /// and so is never evicted, until a matching [`Engine::close`].
    pub async fn open(&self, folder: FolderId, path: &str) -> Result<PathBuf> {
        let handle = self.inner.folder(&folder)?;
        let _guard = handle.fs_lock.lock().await;
        if self.local_path(folder, path)?.is_none()
            && !materialize::fetch_one(&self.inner, &self.node, folder, &handle, path).await?
        {
            return Err(Error::Unavailable(path.to_owned()));
        }
        *handle
            .pins
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_insert(0) += 1;
        let abs = {
            let mut state = handle.lock();
            state.used.insert(path.to_owned(), now_ms());
            state.config.path.join(path)
        };
        // Opening may have pushed a partial replica over its budget.
        evict::evict(&self.inner, &self.node, folder, &handle).await?;
        Ok(abs)
    }

    /// Releases one [`Engine::open`] of `path`.
    pub fn close(&self, folder: FolderId, path: &str) -> Result<()> {
        let handle = self.inner.folder(&folder)?;
        let mut pins = handle.pins.lock().unwrap();
        if let Some(count) = pins.get_mut(path) {
            *count -= 1;
            if *count == 0 {
                pins.remove(path);
            }
        }
        Ok(())
    }

    /// Replaces or creates the file at `path` with the contents of `source`, which is moved
    /// (or copied, across filesystems) into the folder, and records the change now instead of
    /// at the next scan.
    pub async fn write(&self, folder: FolderId, path: &str, source: &Path) -> Result<()> {
        if !tidefell_proto::sync::is_valid_path(path) {
            return Err(Error::Invalid(format!("invalid path {path:?}")));
        }
        let handle = self.inner.folder(&folder)?;
        {
            let _guard = handle.fs_lock.lock().await;
            let root = handle.lock().config.path.clone();
            let target = root.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(crate::io_err(parent))?;
            }
            if std::fs::rename(source, &target).is_err() {
                let tmp_dir = root.join(crate::scan::RESERVED_DIR).join("tmp");
                std::fs::create_dir_all(&tmp_dir).map_err(crate::io_err(&tmp_dir))?;
                let tmp = tmp_dir.join(format!(
                    "write-{:016x}",
                    u64::from_le_bytes(crate::random_bytes()?)
                ));
                std::fs::copy(source, &tmp).map_err(crate::io_err(source))?;
                std::fs::rename(&tmp, &target).map_err(crate::io_err(&target))?;
            }
        }
        run_scan(&self.inner, folder).await
    }

    /// Deleted files in `folder`'s recycle bin, most recently deleted first.
    pub fn recycle_bin(&self, folder: FolderId) -> Result<Vec<RecycledInfo>> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let bin = state.meta.recycle_bin();
        let mut items: Vec<RecycledInfo> = bin
            .iter()
            .map(|r| RecycledInfo {
                entry: r.version.entry,
                path: r.version.path.clone(),
                size: match r.version.kind {
                    EntryKind::File { size, .. } => size,
                    _ => 0,
                },
                deleted_at_ms: r.deleted_at_ms,
                expires_at_ms: bin.expires_at_ms(r),
            })
            .collect();
        items.sort_by_key(|i| std::cmp::Reverse(i.deleted_at_ms));
        Ok(items)
    }

    /// Restores a deleted entry from the recycle bin. The file reappears on every replica,
    /// including this one once the materialiser runs.
    pub fn restore(&self, folder: FolderId, entry: EntryId) -> Result<()> {
        let handle = self.inner.folder(&folder)?;
        {
            let mut state = handle.lock();
            state
                .meta
                .restore(self.inner.key.signing_key(), entry, now_ms())?;
            state.save()?;
        }
        self.inner.emit(Event::FolderChanged(folder));
        self.inner.changed();
        Ok(())
    }

    /// A summary of `folder`.
    pub fn status(&self, folder: FolderId) -> Result<FolderStatus> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
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
        let partial = matches!(state.config.mode, crate::ReplicaMode::Partial { .. });
        let materialised: HashSet<EntryId> = state.local.values().map(|l| l.entry).collect();
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
                // Placeholders of a partial replica are not pending.
                if !on_disk && (!partial || materialised.contains(&version.entry)) {
                    status.pending += 1;
                }
            }
        }
        Ok(status)
    }

    // ---- Peers ----

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

async fn run_evict(inner: &Arc<Inner>, node: &Arc<Node>, id: FolderId) -> Result<()> {
    let handle = inner.folder(&id)?;
    let _guard = handle.fs_lock.lock().await;
    evict::evict(inner, node, id, &handle).await
}

async fn run_materialize(inner: &Arc<Inner>, node: &Arc<Node>, id: FolderId) -> Result<()> {
    let handle = inner.folder(&id)?;
    let _guard = handle.fs_lock.lock().await;
    materialize::materialize(inner, node, id, &handle).await
}

fn purge(inner: &Inner, now_ms: i64) {
    let handles: Vec<(FolderId, Arc<FolderHandle>)> = inner
        .folders
        .read()
        .unwrap()
        .iter()
        .map(|(id, h)| (*id, h.clone()))
        .collect();
    let mut orphaned = BTreeSet::new();
    for (id, handle) in &handles {
        let mut state = handle.lock();
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
        handles
            .iter()
            .any(|(_, h)| h.lock().index.is_referenced(hash))
    };
    let mut store = inner.store.lock().unwrap();
    for hash in orphaned {
        if !still_used(&hash) {
            let _ = store.remove(&hash);
        }
    }
}
