//! The running engine: shared state, background tasks and the public handle.
//!
//! The handle's operations are split by concern: [`folders`], [`files`], [`recycle`] and
//! [`peers`].

mod bundle;
mod conflicts;
mod doctor;
mod files;
mod folders;
mod history;
pub(crate) mod metrics;
mod peers;
mod recycle;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
use tracing::{debug, error, info, warn};

pub use bundle::{zip_bundle, BundleEntry, BundleOptions};
pub use conflicts::Resolution;
pub use folders::FolderStatus;

use crate::backend::EngineBackend;
use crate::config::{EngineConfig, FolderConfig, PeerAddr};
use crate::runtime::Runtime;
use crate::scan::Scope;
use crate::state::FolderState;
use crate::views::Event;
use crate::watch::{FolderWatch, Wanted};
use crate::{copies, evict, materialize, root, scan, Error, Result};
use peers::endpoint_addr;
use recycle::purge;

/// How often to retry pending downloads and reconnect to peers.
const RETRY_INTERVAL: Duration = Duration::from_secs(5);
/// How often a watched folder is scanned in full anyway, in case the watch missed a change.
const SAFETY_SCAN: Duration = Duration::from_secs(3600);
/// How often an unavailable folder's root is checked again, at most (ADR 0037).
const RECHECK_INTERVAL: Duration = Duration::from_secs(60);
/// How often to purge expired recycle-bin items.
const PURGE_INTERVAL: Duration = Duration::from_secs(3600);
/// How long after starting a database too large to check when opened is checked (ADR 0035).
const DEFERRED_CHECK: Duration = Duration::from_secs(60);
/// How often to ask full replicas about files that have no confirmed copy yet (ADR 0022).
const CONFIRM_INTERVAL: Duration = Duration::from_secs(10);

/// One folder: its state, a lock serialising disk work, and its scanner.
pub(crate) struct FolderHandle {
    state: Mutex<FolderState>,
    /// Serialises scanning and materialising, so neither sees the other's half-finished work.
    pub(crate) fs_lock: tokio::sync::Mutex<()>,
    /// What the folder's status shows that takes a pass over every entry, and the revision of
    /// the state it was made from: made again only after a change.
    pub(crate) summary: Mutex<Option<(u64, folders::Summary)>>,
    /// True if the folder comes from the configuration file.
    configured: bool,
    scanner: Mutex<Option<AbortHandle>>,
    /// Why the folder is not watched for changes and is scanned on its interval instead.
    pub(crate) watch_error: Mutex<Option<String>>,
    /// Why the folder's root cannot be used (ADR 0037): no scans, writes or deletes until it is
    /// back.
    pub(crate) unavailable: Mutex<Option<String>>,
    /// Paths held open by front-ends, with how many times; never evicted while open.
    open: Mutex<HashMap<String, usize>>,
    /// Scans, exchanges and problems, for status displays (ADR 0030).
    pub(crate) activity: Mutex<crate::activity::Activity>,
    /// Asks the scanner for a full scan now, as when a folder being rebuilt gets entries to
    /// match its files to (ADR 0034).
    pub(crate) rescan: Notify,
}

impl FolderHandle {
    pub(crate) fn lock(&self) -> MutexGuard<'_, FolderState> {
        self.state.lock().unwrap()
    }

    /// Checks the folder's root and marker, recording why the folder is unavailable if they are
    /// not there, as when its drive is not mounted.
    pub(crate) fn available(&self) -> Result<()> {
        let (id, root) = {
            let state = self.lock();
            (state.config.id, state.config.path.clone())
        };
        let reason = root::check(&root, id);
        let mut unavailable = self.unavailable.lock().unwrap();
        match (&*unavailable, &reason) {
            (None, Some(reason)) => {
                warn!(folder = %id, reason = %reason, "folder unavailable; not syncing it");
            }
            (Some(_), None) => {
                info!(folder = %id, "folder available again");
                crate::history::recover(&root);
            }
            _ => {}
        }
        unavailable.clone_from(&reason);
        match reason {
            Some(reason) => Err(Error::FolderUnavailable(id, reason)),
            None => Ok(()),
        }
    }

    /// Takes the file lock, once the folder's root is available.
    pub(crate) async fn disk(&self) -> Result<tokio::sync::MutexGuard<'_, ()>> {
        let guard = self.fs_lock.lock().await;
        self.available()?;
        Ok(guard)
    }

    /// Paths currently held open.
    pub(crate) fn open_paths(&self) -> HashSet<String> {
        self.open.lock().unwrap().keys().cloned().collect()
    }
}

/// State shared by the engine's tasks and its network backend.
pub(crate) struct Inner {
    pub(crate) key: DeviceKey,
    pub(crate) certificate: Signed<DeviceCertificate>,
    /// The user this device belongs to, from its certificate.
    pub(crate) user: UserId,
    pub(crate) data_dir: PathBuf,
    /// Where the folders' databases are (ADR 0034).
    pub(crate) metadata_dir: PathBuf,
    /// Which volumes are below their free-space floor (ADR 0037).
    pub(crate) floors: crate::space::Floors,
    pub(crate) store: Mutex<BlockStore>,
    /// Contents being downloaded, whose chunks the block store keeps until they are written.
    pub(crate) fetching: Mutex<HashSet<tidefell_proto::ContentHash>>,
    /// Chunk requests waiting to go to each peer together (ADR 0032).
    pub(crate) batches: Arc<crate::batch::Batches>,
    /// Whether each peer said it is a full replica of each folder, and when (ADR 0032).
    pub(crate) full_replicas: Mutex<HashMap<(DeviceId, FolderId), (bool, std::time::Instant)>>,
    /// Held while asking a peer whether it is a full replica, so downloads starting together
    /// ask once.
    pub(crate) replica_query: tokio::sync::Mutex<()>,
    folders: RwLock<BTreeMap<FolderId, Arc<FolderHandle>>>,
    /// Folders that could not be opened when the engine started, with their configuration,
    /// whether it is from the configuration file, and why: the others start regardless, and a
    /// folder added at runtime stays in the runtime settings (ADR 0034).
    pub(crate) unopened: Mutex<BTreeMap<FolderId, (FolderConfig, bool, String)>>,
    /// The pins of folders in the runtime settings that are not open, kept as they were.
    other_pins: Mutex<BTreeMap<FolderId, Vec<String>>>,
    configured_peers: Vec<PeerAddr>,
    runtime_peers: RwLock<Vec<PeerAddr>>,
    pub(crate) changes: watch::Sender<u64>,
    pub(crate) materialize: Notify,
    events: broadcast::Sender<Event>,
    /// Serialises changes to the folder set and their persistence.
    runtime_lock: Mutex<()>,
    /// Known devices' certificates, and authors not vouched for (ADR 0024).
    pub(crate) vouching: Mutex<crate::vouch::Vouching>,
    /// Watch folders for changes instead of only scanning them on an interval (ADR 0029).
    watch: bool,
    /// Set once a task panics: the engine has stopped working (ADR 0030).
    failure: watch::Sender<Option<String>>,
    /// Syncing paused for the whole device, and for these folders (ADR 0029).
    paused: RwLock<(bool, BTreeSet<FolderId>)>,
    /// The configuration the engine was started with, for diagnostics bundles (ADR 0030).
    diagnostics_config: Mutex<Option<serde_json::Value>>,
    /// The issue tracker crash reports go to, if not the default (ADR 0030).
    report_url: Mutex<Option<String>>,
    /// Counted as they happen, for metrics (ADR 0030).
    pub(crate) counters: crate::engine::metrics::Counters,
    /// [`Inner::retry_interval`], in milliseconds.
    retry_ms: std::sync::atomic::AtomicU64,
    /// This device's time, shared with its folders.
    pub(crate) clock: Arc<crate::clock::Clock>,
    /// Chunks the records say a file holds that could not be read from it: fetched, not
    /// counted as here, until then.
    pub(crate) unreadable: Mutex<HashSet<tidefell_proto::ChunkHash>>,
}

impl Inner {
    /// Milliseconds since the Unix epoch, as this device tells it.
    pub(crate) fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// How often pending downloads are retried and peers dialled again.
    pub(crate) fn retry_interval(&self) -> Duration {
        Duration::from_millis(self.retry_ms.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Records that the task `task` panicked. Locks it held may be poisoned and state in memory
    /// may be inconsistent, so the engine stops working and its owner is told (ADR 0030).
    /// Returns what went wrong.
    pub(crate) fn fail(&self, task: &str, panic: Box<dyn std::any::Any + Send>) -> String {
        let message = panic
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic without a message".into());
        let message = format!("the {task} task panicked: {message}");
        error!(task, "{message}; the engine has stopped");
        self.failure.send_if_modified(|failure| {
            if failure.is_some() {
                return false;
            }
            *failure = Some(message.clone());
            true
        });
        self.emit(Event::Crashed(message.clone()));
        message
    }

    /// Records whether `peer` said it is a full replica of `folder` (ADR 0032).
    pub(crate) fn remember_replica(&self, peer: DeviceId, folder: FolderId, full: bool) {
        self.full_replicas
            .lock()
            .unwrap()
            .insert((peer, folder), (full, std::time::Instant::now()));
    }

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
    /// Whether `folder` transfers nothing: it, or the whole device, is paused (ADR 0029).
    pub(crate) fn is_paused(&self, folder: &FolderId) -> bool {
        let paused = self.paused.read().unwrap();
        paused.0 || paused.1.contains(folder)
    }

    pub(crate) fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
        self.materialize.notify_one();
    }

    /// Tells peers about the versions each commit of `handle`'s folder makes visible
    /// (ADR 0034), from now on.
    pub(crate) fn watch_commits(self: &Arc<Self>, handle: &FolderHandle) {
        let inner = Arc::downgrade(self);
        handle.lock().on_commit(Box::new(move || {
            if let Some(inner) = inner.upgrade() {
                inner.changes.send_modify(|n| *n += 1);
            }
        }));
    }

    pub(crate) fn emit(&self, event: Event) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }

    pub(crate) fn peers(&self) -> Vec<PeerAddr> {
        let mut peers = self.configured_peers.clone();
        for peer in self.runtime_peers.read().unwrap().iter() {
            if !peers.iter().any(|p| p.device == peer.device) {
                peers.push(peer.clone());
            }
        }
        peers
    }

    /// Writes runtime folders, peers, pause and pins. Callers hold `runtime_lock`, and no
    /// folder's lock.
    pub(crate) fn save_runtime(&self) -> Result<()> {
        let folders = self.folders.read().unwrap();
        let unopened = self.unopened.lock().unwrap();
        let mut pins: BTreeMap<FolderId, Vec<String>> = self.other_pins.lock().unwrap().clone();
        for (id, handle) in folders.iter() {
            pins.insert(*id, handle.lock().pins.iter().cloned().collect());
        }
        let (paused, paused_folders) = self.paused.read().unwrap().clone();
        let runtime = Runtime {
            folders: folders
                .values()
                .filter(|h| !h.configured)
                .map(|h| (&h.lock().config).into())
                .chain(
                    unopened
                        .values()
                        .filter(|(_, configured, _)| !configured)
                        .map(|(config, _, _)| config.into()),
                )
                .collect(),
            peers: self
                .runtime_peers
                .read()
                .unwrap()
                .iter()
                .map(Into::into)
                .collect(),
            paused,
            paused_folders: paused_folders.into_iter().collect(),
            pins: pins.into_iter().collect(),
            certificates: self
                .vouching
                .lock()
                .unwrap()
                .certificates()
                .cloned()
                .collect(),
        };
        runtime.save(&self.data_dir)
    }

    /// Keeps a device's certificate, if it is new or newer than the one held (ADR 0024).
    pub(crate) fn learn_certificate(&self, certificate: Signed<DeviceCertificate>) {
        if !self.vouching.lock().unwrap().learn(certificate) {
            return;
        }
        let _guard = self.runtime_lock.lock().unwrap();
        if let Err(e) = self.save_runtime() {
            warn!(error = %e, "saving a learnt certificate failed");
        }
    }
}

/// A running engine.
pub struct Engine {
    inner: Arc<Inner>,
    node: Arc<Node>,
    tasks: JoinSet<()>,
    local_discovery: bool,
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
    metadata_dir: &Path,
    config: FolderConfig,
    device: DeviceId,
    configured: bool,
    open: crate::state::Open,
) -> Result<FolderHandle> {
    // Only a folder being created or joined gets its root created (ADR 0037); an existing
    // folder whose root is missing is unavailable, not empty.
    let new = !FolderState::exists(data_dir, metadata_dir, config.id);
    if new {
        root::create(&config.path, config.id, device)?;
    }
    let state = FolderState::open_or_rebuild(data_dir, metadata_dir, config, device, open)?;
    if !new {
        if let Err(e) = root::adopt(&state, device) {
            warn!(folder = %state.config.id, error = %e, "marking the folder's root failed");
        }
    }
    let (id, root) = (state.config.id, state.config.path.clone());
    let unavailable = root::check(&root, id);
    let mut state = state;
    // What the drive does with names, so files are written where it can keep them apart
    // (ADR 0033). A folder that cannot be probed, such as an unavailable one, is assumed to
    // keep every name.
    if unavailable.is_none() {
        match crate::probe::probe(&root) {
            Ok(filesystem) => {
                debug!(folder = %id, filesystem = %filesystem.describe(), "probed the folder's drive");
                state.set_filesystem(filesystem);
            }
            Err(e) => warn!(folder = %id, error = %e, "cannot probe the folder's drive"),
        }
    }
    match &unavailable {
        // Before the first scan, which would otherwise record the file as deleted.
        None => crate::history::recover(&root),
        Some(reason) => warn!(folder = %id, reason = %reason, "folder unavailable; not syncing it"),
    }
    Ok(FolderHandle {
        state: Mutex::new(state),
        fs_lock: tokio::sync::Mutex::new(()),
        configured,
        scanner: Mutex::new(None),
        watch_error: Mutex::new(None),
        unavailable: Mutex::new(unavailable),
        open: Mutex::new(HashMap::new()),
        activity: Mutex::default(),
        rescan: Notify::new(),
        summary: Mutex::new(None),
    })
}

/// The network an engine runs on.
enum Network {
    Iroh,
    /// Simulated, for the harness of ADR 0036.
    #[cfg(feature = "simulation")]
    Memory(tidefell_net::Network),
}

/// Runs `task`, marking the engine failed if it panics instead of the loop silently ending.
fn supervised(
    inner: Arc<Inner>,
    name: &'static str,
    task: impl std::future::Future<Output = ()> + Send + 'static,
) -> impl std::future::Future<Output = ()> + Send + 'static {
    on_panic(task, move |e| {
        inner.fail(name, e);
    })
}

/// Runs `task` and calls `panicked` with the payload if it panics. The task is polled in place,
/// not spawned, so aborting or shutting down the caller stops it before that returns: a
/// restarted engine must never share its folders with the old one's loops. Nothing is reused
/// after a panic, since the engine is marked failed.
async fn on_panic(
    task: impl std::future::Future<Output = ()> + Send + 'static,
    panicked: impl FnOnce(Box<dyn std::any::Any + Send>) + Send + 'static,
) {
    let mut task = Box::pin(task);
    let outcome = std::future::poll_fn(|cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| task.as_mut().poll(cx))) {
            Ok(std::task::Poll::Ready(())) => std::task::Poll::Ready(Ok(())),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(panic) => std::task::Poll::Ready(Err(panic)),
        }
    })
    .await;
    if let Err(panic) = outcome {
        panicked(panic);
    }
}

fn spawn_scanner(inner: &Arc<Inner>, id: FolderId, handle: &FolderHandle) {
    let (interval, root) = {
        let state = handle.lock();
        (state.config.scan_interval, state.config.path.clone())
    };
    let task = {
        let inner = inner.clone();
        tokio::spawn(supervised(inner.clone(), "scan", async move {
            let mut watch = None;
            // Set until the folder is watched, or cannot be; again while it is unavailable, so
            // a drive mounted over an empty directory is watched once it is back (ADR 0037).
            let mut unwatched = inner.watch;
            let mut wanted = Wanted::Full;
            loop {
                // Watched before scanning, so nothing changed during the scan is missed.
                if unwatched && inner.folder(&id).is_ok_and(|h| h.available().is_ok()) {
                    unwatched = false;
                    match FolderWatch::new(&root) {
                        Ok(w) => {
                            watch = Some(w);
                            if let Ok(h) = inner.folder(&id) {
                                h.watch_error.lock().unwrap().take();
                            }
                        }
                        Err(e) => not_watched(&inner, id, e),
                    }
                }
                let scope = match &wanted {
                    Wanted::Full => None,
                    Wanted::Scope(scope) => Some(scope.clone()),
                };
                match run_scan(&inner, id, scope, None).await {
                    Ok(dirs) => {
                        if let Some(w) = &mut watch {
                            if let Err(e) = w.add(&dirs) {
                                not_watched(&inner, id, e);
                                watch = None;
                            }
                        }
                    }
                    Err(Error::FolderUnavailable(..)) => {
                        watch = None;
                        unwatched = inner.watch;
                    }
                    Err(e) => warn!(folder = %id, error = %e, "scan failed"),
                }
                let Ok(handle) = inner.folder(&id) else {
                    return;
                };
                let rescan = handle.rescan.notified();
                wanted = match &mut watch {
                    Some(w) => {
                        let ignores = handle.lock().ignores.clone();
                        tokio::select! {
                            () = rescan => Wanted::Full,
                            next = w.next(&ignores) => match next {
                                Some(wanted) => wanted,
                                None => {
                                    not_watched(&inner, id, "the watch stopped".into());
                                    watch = None;
                                    Wanted::Full
                                }
                            },
                            () = tokio::time::sleep(SAFETY_SCAN) => Wanted::Full,
                        }
                    }
                    None => {
                        let away = inner
                            .folder(&id)
                            .is_ok_and(|h| h.unavailable.lock().unwrap().is_some());
                        let wait = if away {
                            interval.min(RECHECK_INTERVAL)
                        } else {
                            interval
                        };
                        tokio::select! {
                            () = rescan => {}
                            () = tokio::time::sleep(wait) => {}
                        }
                        Wanted::Full
                    }
                };
            }
        }))
    };
    if let Some(old) = handle.scanner.lock().unwrap().replace(task.abort_handle()) {
        old.abort();
    }
}

/// Where the folders' databases go: the configured `metadata_dir`, or `<data_dir>/metadata`
/// (ADR 0034). Fails if they are still where an earlier `metadata_dir` put them, rather than
/// start each folder afresh; records the directory otherwise.
fn metadata_dir(config: &EngineConfig, runtime: &Runtime) -> Result<PathBuf> {
    let default = config.data_dir.join("metadata");
    let dir = config
        .metadata_dir
        .clone()
        .unwrap_or_else(|| default.clone());
    let marker = config.data_dir.join("metadata-dir");
    let previous = std::fs::read_to_string(&marker)
        .ok()
        .map(|p| PathBuf::from(p.trim_end()))
        .unwrap_or(default);
    if previous != dir {
        let ids = config
            .folders
            .iter()
            .map(|f| f.id)
            .chain(runtime.folders.iter().map(|f| f.id));
        for id in ids {
            let (here, there) = (
                crate::state::db_path(&dir, id),
                crate::state::db_path(&previous, id),
            );
            if !here.exists() && there.exists() {
                return Err(Error::MetadataLeftBehind {
                    from: previous,
                    to: dir,
                });
            }
        }
    }
    std::fs::create_dir_all(&dir).map_err(crate::io_err(&dir))?;
    if previous != dir || !marker.exists() {
        std::fs::write(&marker, dir.to_string_lossy().as_bytes())
            .map_err(crate::io_err(&marker))?;
    }
    Ok(dir)
}

impl Engine {
    /// Opens state under `config.data_dir`, binds the network endpoint and starts scanning,
    /// syncing and materialising every configured and previously added folder.
    pub async fn start(
        config: EngineConfig,
        key: DeviceKey,
        certificate: Signed<DeviceCertificate>,
    ) -> Result<Self> {
        Self::start_on(config, key, certificate, Network::Iroh).await
    }

    /// Starts an engine as [`Engine::start`] does, on the simulated `network` instead of iroh
    /// (ADR 0036): it reaches only engines on that network, by device. `config`'s relay, bind
    /// and discovery settings do not apply.
    #[cfg(feature = "simulation")]
    pub async fn start_simulated(
        config: EngineConfig,
        key: DeviceKey,
        certificate: Signed<DeviceCertificate>,
        network: &tidefell_net::Network,
    ) -> Result<Self> {
        Self::start_on(config, key, certificate, Network::Memory(network.clone())).await
    }

    async fn start_on(
        config: EngineConfig,
        key: DeviceKey,
        certificate: Signed<DeviceCertificate>,
        network: Network,
    ) -> Result<Self> {
        let device = key.id();
        let user = certificate
            .verify_certificate()
            .map_err(|e| Error::Invalid(format!("device certificate: {e}")))?
            .user;
        std::fs::create_dir_all(&config.data_dir).map_err(crate::io_err(&config.data_dir))?;
        let store = BlockStore::open(config.data_dir.join("blocks"))?;
        let runtime = Runtime::load(&config.data_dir)?;
        let metadata_dir = metadata_dir(&config, &runtime)?;

        // The configuration file wins over a folder added at runtime with the same ID. One
        // folder that cannot be opened does not stop the others (ADR 0034).
        let mut folders = BTreeMap::new();
        let mut unopened = BTreeMap::new();
        let runtime_folders = runtime
            .folders
            .into_iter()
            .map(|f| (FolderConfig::from(f), false));
        for (folder, configured) in config
            .folders
            .iter()
            .map(|f| (f.clone(), true))
            .chain(runtime_folders)
        {
            let id = folder.id;
            if folders.contains_key(&id) || unopened.contains_key(&id) {
                continue;
            }
            match open_folder(
                &config.data_dir,
                &metadata_dir,
                folder.clone(),
                device,
                configured,
                crate::state::Open::Normal,
            ) {
                Ok(handle) => {
                    folders.insert(id, Arc::new(handle));
                }
                Err(e) => {
                    error!(folder = %id, error = %e, "cannot open the folder; not syncing it");
                    unopened.insert(id, (folder, configured, e.to_string()));
                }
            }
        }
        // Pins are in the runtime settings (ADR 0034). A folder without an entry there still
        // has them in its metadata, from before: they move to the runtime settings now.
        let mut other_pins: BTreeMap<FolderId, Vec<String>> = runtime.pins.into_iter().collect();
        let mut moving_pins = false;
        for (id, handle) in &folders {
            match other_pins.remove(id) {
                Some(pins) => {
                    let mut state = handle.lock();
                    state.pins = pins.into_iter().collect();
                    state.touch();
                }
                None => moving_pins = true,
            }
        }

        let inner = Arc::new(Inner {
            key: key.clone(),
            certificate,
            user,
            data_dir: config.data_dir.clone(),
            metadata_dir: metadata_dir.clone(),
            floors: crate::space::Floors::new(config.min_free_space),
            store: Mutex::new(store),
            fetching: Mutex::new(HashSet::new()),
            batches: Arc::default(),
            full_replicas: Mutex::default(),
            replica_query: tokio::sync::Mutex::default(),
            counters: crate::engine::metrics::Counters::default(),
            folders: RwLock::new(folders),
            unopened: Mutex::new(unopened),
            other_pins: Mutex::new(other_pins),
            configured_peers: config.peers.clone(),
            runtime_peers: RwLock::new(runtime.peers.into_iter().map(Into::into).collect()),
            changes: watch::channel(0).0,
            materialize: Notify::new(),
            events: broadcast::channel(256).0,
            runtime_lock: Mutex::new(()),
            vouching: Mutex::new(crate::vouch::Vouching::new(runtime.certificates)),
            watch: config.watch,
            failure: watch::channel(None).0,
            paused: RwLock::new((runtime.paused, runtime.paused_folders.into_iter().collect())),
            diagnostics_config: Mutex::new(None),
            report_url: Mutex::new(None),
            retry_ms: std::sync::atomic::AtomicU64::new(RETRY_INTERVAL.as_millis() as u64),
            clock: Arc::default(),
            unreadable: Mutex::default(),
        });
        for handle in inner.folders.read().unwrap().values() {
            handle.lock().clock = inner.clock.clone();
        }

        let backend = Arc::new(EngineBackend(inner.clone()));
        let node = Arc::new(match network {
            Network::Iroh => {
                Node::spawn(
                    NodeConfig {
                        secret_key: key.seed(),
                        relay: config.relay.clone(),
                        address_lookup: config.address_lookup,
                        local_discovery: config.local_discovery,
                        limits: config.limits,
                        bind: config.bind,
                    },
                    backend,
                )
                .await?
            }
            #[cfg(feature = "simulation")]
            Network::Memory(network) => {
                Node::spawn_simulated(&network, device, config.limits, backend)
            }
        });
        info!(device = %device, "engine started");

        if moving_pins {
            inner.save_runtime()?;
        }
        for (id, handle) in inner.folders.read().unwrap().iter() {
            inner.watch_commits(handle);
            spawn_scanner(&inner, *id, handle);
        }

        let mut tasks = JoinSet::new();
        {
            let (inner, node) = (inner.clone(), node.clone());
            tasks.spawn(supervised(inner.clone(), "materialise", async move {
                loop {
                    for id in inner.folder_ids() {
                        // Eviction is local disk work and carries on while paused.
                        let result = match inner.is_paused(&id) {
                            true => Ok(()),
                            false => run_materialize(&inner, &node, id).await,
                        };
                        match result {
                            Ok(())
                            | Err(Error::UnknownFolder(_) | Error::FolderUnavailable(..)) => {}
                            Err(e) => warn!(folder = %id, error = %e, "materialise failed"),
                        }
                        match run_evict(&inner, id).await {
                            Ok(())
                            | Err(Error::UnknownFolder(_) | Error::FolderUnavailable(..)) => {}
                            Err(e) => warn!(folder = %id, error = %e, "eviction failed"),
                        }
                    }
                    // Written files no longer need their chunks in the block store; the first
                    // pass also releases what earlier versions kept a second copy of (ADR 0027).
                    let work = !inner.store.lock().unwrap().is_empty()
                        || inner.handles().iter().any(|h| !h.lock().kept.is_empty());
                    if work {
                        let inner = inner.clone();
                        let compacted =
                            tokio::task::spawn_blocking(move || crate::content::compact(&inner))
                                .await;
                        if let Err(e) = compacted {
                            if e.is_panic() {
                                std::panic::resume_unwind(e.into_panic());
                            }
                        }
                    }
                    tokio::select! {
                        () = inner.materialize.notified() => {}
                        () = tokio::time::sleep(inner.retry_interval()) => {}
                    }
                }
            }));
        }
        {
            let (inner, node) = (inner.clone(), node.clone());
            tasks.spawn(supervised(inner.clone(), "dial", async move {
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
                    tokio::time::sleep(inner.retry_interval()).await;
                }
            }));
        }
        {
            let (inner, node) = (inner.clone(), node.clone());
            tasks.spawn(supervised(inner.clone(), "confirm", async move {
                loop {
                    tokio::time::sleep(CONFIRM_INTERVAL).await;
                    for id in inner
                        .folder_ids()
                        .into_iter()
                        .filter(|id| !inner.is_paused(id))
                    {
                        let Ok(handle) = inner.folder(&id) else {
                            continue;
                        };
                        if let Err(e) = copies::confirm(&inner, &node, id, &handle).await {
                            warn!(folder = %id, error = %e, "confirming copies failed");
                        }
                    }
                }
            }));
        }
        {
            let inner = inner.clone();
            tasks.spawn(supervised(inner.clone(), "purge", async move {
                // Databases too large to check when opened are checked soon after (ADR 0035).
                tokio::time::sleep(DEFERRED_CHECK).await;
                check_databases(&inner, true).await;
                let mut hours = 0u32;
                loop {
                    tokio::time::sleep(PURGE_INTERVAL).await;
                    purge(&inner, inner.now_ms());
                    hours += 1;
                    if hours.is_multiple_of(24) {
                        check_databases(&inner, false).await;
                    }
                }
            }));
        }
        Ok(Self {
            inner,
            node,
            tasks,
            local_discovery: config.local_discovery,
        })
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

    /// What stopped the engine, if a task panicked (ADR 0030). Once set, the engine does no more
    /// work and its owner should shut it down: `tidefelld` exits so its service manager restarts
    /// it, and the bindings stop the node.
    pub fn failure(&self) -> Option<String> {
        self.inner.failure.borrow().clone()
    }

    /// Resolves when a task panics, with what went wrong, or with `None` once the engine has shut
    /// down. It holds no reference to the engine, so waiting does not keep it alive.
    pub fn failed(&self) -> impl std::future::Future<Output = Option<String>> + Send + 'static {
        let mut failure = self.inner.failure.subscribe();
        async move {
            failure
                .wait_for(Option::is_some)
                .await
                .ok()
                .and_then(|m| m.clone())
        }
    }

    /// The engine's state directory.
    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    /// Where each folder's metadata database is kept (ADR 0034).
    pub fn metadata_dir(&self) -> &Path {
        &self.inner.metadata_dir
    }

    /// Whether this device announces itself and finds known devices on the local network.
    pub fn local_discovery(&self) -> bool {
        self.local_discovery
    }

    /// The upload and download limits in force (ADR 0029).
    pub fn limits(&self) -> crate::Limits {
        self.node.limits()
    }

    /// Changes the upload and download limits until the engine stops. Rates below
    /// [`crate::MIN_BYTES_PER_SEC`] are raised to it.
    pub fn set_limits(&self, limits: crate::Limits) {
        self.node.set_limits(limits);
    }

    /// What the filesystem of `folder` does with names, as probed when the folder opened
    /// (ADR 0033).
    pub fn filesystem(&self, folder: FolderId) -> Result<crate::Filesystem> {
        Ok(self.inner.folder(&folder)?.lock().filesystem.clone())
    }

    /// Treats `folder` as if its filesystem did what `filesystem` says with names, instead of
    /// what the probe found: for tests, and for a drive the probe cannot judge (ADR 0033).
    pub fn assume_filesystem(&self, folder: FolderId, filesystem: crate::Filesystem) -> Result<()> {
        self.inner
            .folder(&folder)?
            .lock()
            .set_filesystem(filesystem);
        self.inner.changes.send_modify(|n| *n += 1);
        Ok(())
    }

    /// Sets this engine's clock `offset` ahead of the system's, or behind if negative, as a
    /// device whose clock is wrong would have it. For simulations, where several engines share
    /// a process and so a system clock (ADR 0036).
    pub fn set_clock_offset(&self, offset_ms: i64) {
        self.inner.clock.set_offset(offset_ms);
    }

    /// Changes how often this engine retries pending downloads and dials peers it is not
    /// connected to: every five seconds unless set. Tests and simulations shorten it, so a
    /// healed network reconnects at once (ADR 0036).
    pub fn set_retry_interval(&self, interval: Duration) {
        let ms = u64::try_from(interval.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        self.inner
            .retry_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Adds a fixed delay before every request this engine sends to a peer, as a slow link's
    /// round trips would, to measure what they cost (ADR 0032). Zero unless set.
    pub fn set_request_delay(&self, delay: Duration) {
        self.node.set_request_delay(delay);
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
        run_scan(&self.inner, folder, None, None).await.map(drop)
    }

    /// Tries again every write and delete in `folder` that failed in a way that lasts, such as
    /// permission refused, rather than waiting for the file to change (ADR 0037).
    pub fn retry(&self, folder: FolderId) -> Result<()> {
        self.inner
            .folder(&folder)?
            .activity
            .lock()
            .unwrap()
            .stuck
            .clear();
        self.inner.materialize.notify_one();
        Ok(())
    }

    /// Keeps `bytes` free on every drive the engine writes to from now on, or 2% of each up to
    /// 1 GiB if `None` (ADR 0037). Downloads to a drive below it wait.
    pub fn set_min_free_space(&self, bytes: Option<u64>) {
        self.inner.floors.set(bytes);
        // Writes that waited are tried again.
        self.inner.changed();
    }

    /// Materialises remote changes to `folder` now.
    pub async fn materialize(&self, folder: FolderId) -> Result<()> {
        run_materialize(&self.inner, &self.node, folder).await
    }

    /// The certificates of the devices this one knows: its own, its peers' and those of authors
    /// it has vouched for (ADR 0024).
    pub fn certificates(&self) -> Vec<Signed<DeviceCertificate>> {
        let vouching = self.inner.vouching.lock().unwrap();
        std::iter::once(&self.inner.certificate)
            .chain(vouching.certificates())
            .cloned()
            .collect()
    }

    /// Stops all tasks, closes the endpoint and saves state.
    pub async fn shutdown(mut self) {
        self.tasks.shutdown().await;
        // Stop serving before the state is saved for the last time. A version stored after it
        // would be lost, and peers that saw it acknowledge index sequences this device then
        // uses again for its next changes, which they would never ask for.
        // Tasks of their own, such as a batch of requests, may still hold the node.
        self.node.close().await;
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
            if let Err(e) = handle.lock().close() {
                warn!(folder = %id, error = %e, "saving state on shutdown failed");
            }
        }
    }
}

/// Checks each folder's database for damage, as every day (ADR 0034), rebuilding a folder whose
/// database cannot be repaired: every folder's, or only those not checked since they were
/// opened (`unchecked`). Returns what each check found.
pub(crate) async fn check_databases(
    inner: &Arc<Inner>,
    unchecked: bool,
) -> Vec<(FolderId, Result<()>)> {
    let mut found = Vec::new();
    for id in inner.folder_ids() {
        let Some(waiter) = inner.folder(&id).ok().and_then(|h| {
            let state = h.lock();
            (!unchecked || state.unchecked())
                .then(|| state.committer().map(|c| c.waiter()))
                .flatten()
        }) else {
            continue;
        };
        let checked = tokio::task::spawn_blocking(move || waiter.check())
            .await
            .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()));
        if checked.is_ok() {
            if let Ok(handle) = inner.folder(&id) {
                handle.lock().mark_checked();
            }
        }
        // Opened again, the database is checked afresh, and the folder rebuilt only if it
        // still fails: a check that failed on the open database alone costs nothing.
        if let Err(Error::CorruptState(_)) = &checked {
            warn!(folder = %id, "the folder's database failed its check; opening it again");
            if let Err(e) = reopen_folder(inner, id, crate::state::Open::Recheck).await {
                error!(folder = %id, error = %e, "opening the folder again failed");
            }
        }
        found.push((id, checked));
    }
    found
}

/// Starts `id` over from what is on disk and what peers send (ADR 0034): its database is set
/// aside and a new one made, with what was kept restored from sidecars.
pub(crate) async fn rebuild_folder(inner: &Arc<Inner>, id: FolderId, reason: &str) -> Result<()> {
    reopen_folder(inner, id, crate::state::Open::Rebuild(reason.to_owned())).await
}

/// Closes `id` and opens it again, rebuilding it because of `rebuild`, or if its database is
/// found damaged when it opens.
async fn reopen_folder(inner: &Arc<Inner>, id: FolderId, open: crate::state::Open) -> Result<()> {
    let old = inner.folder(&id)?;
    if let Some(scanner) = old.scanner.lock().unwrap().take() {
        scanner.abort();
    }
    // No scan or write is half done while the folder is swapped.
    let _guard = old.fs_lock.lock().await;
    let (config, pins) = {
        let mut state = old.lock();
        if let Err(e) = state.close() {
            warn!(folder = %id, error = %e, "saving the folder before rebuilding it failed");
        }
        // Tasks still holding the old folder save nothing more.
        state.mark_removed();
        // Pins are this device's choice, not metadata: a rebuilt folder keeps them.
        (state.config.clone(), state.pins.clone())
    };
    let (data_dir, metadata_dir, device) = (
        inner.data_dir.clone(),
        inner.metadata_dir.clone(),
        inner.key.id(),
    );
    let configured = old.configured;
    let handle = tokio::task::spawn_blocking(move || {
        open_folder(&data_dir, &metadata_dir, config, device, configured, open)
    })
    .await
    .unwrap_or_else(|e| std::panic::resume_unwind(e.into_panic()))?;
    {
        let mut state = handle.lock();
        state.pins = pins;
        state.clock = inner.clock.clone();
        state.touch();
    }
    let handle = Arc::new(handle);
    inner.folders.write().unwrap().insert(id, handle.clone());
    inner.watch_commits(&handle);
    spawn_scanner(inner, id, &handle);
    inner.emit(Event::FolderChanged(id));
    // Sessions exchange the folder's index again, from the start.
    inner.changed();
    Ok(())
}

/// Records that folder `id` is scanned on its interval instead of watched, and why.
fn not_watched(inner: &Inner, id: FolderId, reason: String) {
    warn!(folder = %id, reason = %reason, "not watching for changes; scanning on an interval");
    if let Ok(handle) = inner.folder(&id) {
        *handle.watch_error.lock().unwrap() = Some(reason);
    }
}

/// Scans `id`, or only `scope` of it. `saved` is a path just written through the engine,
/// recorded even if ignored. Returns the directories walked.
async fn run_scan(
    inner: &Arc<Inner>,
    id: FolderId,
    scope: Option<Scope>,
    saved: Option<String>,
) -> Result<Vec<PathBuf>> {
    let handle = inner.folder(&id)?;
    let _guard = handle.disk().await?;
    let (inner2, handle2) = (inner.clone(), handle.clone());
    handle.activity.lock().unwrap().scanning = true;
    let started = std::time::Instant::now();
    let covered = scope.clone();
    let result = tokio::task::spawn_blocking(move || {
        scan::scan(&inner2, &handle2, scope.as_ref(), saved.as_deref())
    })
    .await;
    {
        let mut activity = handle.activity.lock().unwrap();
        activity.scanning = false;
        let error = match &result {
            Ok(Err(e)) => Some(e.to_string()),
            Ok(Ok(_)) => None,
            Err(_) => Some("the scan crashed".to_owned()),
        };
        let took = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        activity.last_scan = Some((inner.now_ms(), took, error));
        // A path a scan looked at is tried again, in case what stopped it was fixed there,
        // such as its permissions (ADR 0037).
        let stuck = activity.stuck.len();
        activity
            .stuck
            .retain(|(path, _), _| !covered.as_ref().is_none_or(|s| s.contains(path)));
        if activity.stuck.len() != stuck {
            inner.materialize.notify_one();
        }
    }
    let scanned = match result {
        Ok(scanned) => scanned?,
        Err(e) => {
            let message = inner.fail(
                "scan",
                e.try_into_panic()
                    .unwrap_or_else(|e| Box::new(e.to_string())),
            );
            return Err(Error::Crashed(message));
        }
    };
    if scanned.changed {
        inner.changes.send_modify(|n| *n += 1);
    }
    if scanned.changed || scanned.problems_changed {
        inner.emit(Event::FolderChanged(id));
    }
    for path in scanned.conflicts {
        inner.emit(Event::ConflictDetected { folder: id, path });
    }
    Ok(scanned.dirs)
}

async fn run_evict(inner: &Arc<Inner>, id: FolderId) -> Result<()> {
    let handle = inner.folder(&id)?;
    let _guard = handle.disk().await?;
    evict::evict(inner, id, &handle)
}

/// Brings `id` up to date on disk. Takes the folder's file lock only around disk work, not
/// while fetching from peers.
async fn run_materialize(inner: &Arc<Inner>, node: &Arc<Node>, id: FolderId) -> Result<()> {
    let handle = inner.folder(&id)?;
    // Checked before fetching, so an unavailable folder does not download what it cannot write.
    handle.available()?;
    materialize::materialize(inner, node, id, &handle).await
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[tokio::test]
    async fn a_panicking_task_is_reported() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        on_panic(async { panic!("in a background loop") }, move |panic| {
            let _ = tx.send(panic.downcast_ref::<&str>().copied());
        })
        .await;
        assert_eq!(rx.await.unwrap(), Some("in a background loop"));
    }

    #[tokio::test]
    async fn shutting_down_stops_the_task_before_returning() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let mut tasks = JoinSet::new();
        tasks.spawn(on_panic(
            {
                let guard = Dropped(dropped.clone());
                async move {
                    let _guard = guard;
                    std::future::pending::<()>().await;
                }
            },
            |_| panic!("stopping is not a panic"),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        tasks.shutdown().await;
        // Stopped already, not merely asked to stop.
        assert!(dropped.load(Ordering::SeqCst));
    }
}
