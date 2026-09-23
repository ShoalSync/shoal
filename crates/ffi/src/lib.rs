//! Tidefell `ffi` crate: uniffi bindings → Kotlin (Android), later Swift (ADR 0004).
//!
//! The surface is generated, never hand-written, on the Kotlin side (package `org.tidefell.ffi`):
//!
//! - [`device_id`] and [`is_paired`] inspect a data directory.
//! - A [`Pairing`] shows a pairing code and waits for a device holding the user key to
//!   approve this one (ADR 0015). On success it stores the certificate and the approver.
//! - A [`Node`] runs the engine for a paired device: folders, files (placeholders are opened
//!   on demand, ADR 0016), the recycle bin, peers and events.
//!
//! Every async function runs on a runtime owned by this library, so callers can await them from
//! any executor (Kotlin coroutines on Android).

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use tidefell_engine::pairing::{self, PairingEvent, PairingServer};
use tidefell_engine::{
    device, Engine, EngineConfig, FolderConfig, FolderId, PeerAddr, RelayConfig, ReplicaMode,
    Retention,
};

uniffi::setup_scaffolding!();

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("tidefell")
            .build()
            .expect("tokio runtime")
    })
}

/// Runs `future` on the library's runtime and waits for it from the caller's executor.
async fn on_runtime<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T> {
    runtime()
        .spawn(future)
        .await
        .map_err(|e| TidefellError::Failed {
            message: e.to_string(),
        })
}

/// Errors reported to the app.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum TidefellError {
    /// A folder, file or entry does not exist.
    #[error("{message}")]
    NotFound {
        /// What is missing.
        message: String,
    },
    /// Content or a peer cannot be reached right now.
    #[error("{message}")]
    Unavailable {
        /// What is unavailable.
        message: String,
    },
    /// The request was invalid.
    #[error("{message}")]
    Invalid {
        /// Why.
        message: String,
    },
    /// The device has no certificate yet; pair it first.
    #[error("this device is not paired yet")]
    NotPaired,
    /// Anything else.
    #[error("{message}")]
    Failed {
        /// What failed.
        message: String,
    },
}

impl From<tidefell_engine::Error> for TidefellError {
    fn from(e: tidefell_engine::Error) -> Self {
        use tidefell_engine::Error as E;
        let message = e.to_string();
        match e {
            E::UnknownFolder(_) | E::NoSuchFile(_) => Self::NotFound { message },
            E::Unavailable(_) | E::Net(_) => Self::Unavailable { message },
            E::Invalid(_) | E::FolderExists(_) | E::Configured(_) => Self::Invalid { message },
            _ => Self::Failed { message },
        }
    }
}

type Result<T> = std::result::Result<T, TidefellError>;

fn invalid(message: impl Into<String>) -> TidefellError {
    TidefellError::Invalid {
        message: message.into(),
    }
}

fn parse<T: std::str::FromStr>(what: &str, text: &str) -> Result<T> {
    text.parse()
        .map_err(|_| invalid(format!("{what} {text:?} is not valid")))
}

/// Which relays to use.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum Relay {
    /// The public iroh relays.
    Default,
    /// Self-hosted relays.
    Custom {
        /// Relay URLs.
        urls: Vec<String>,
    },
    /// Direct connections only.
    Disabled,
}

/// How the device reaches peers.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Network {
    /// Relay servers.
    pub relay: Relay,
    /// Publish and resolve addresses through DNS, so peers can be dialed by ID alone.
    pub address_lookup: bool,
    /// Local socket to listen on, e.g. `0.0.0.0:0`; `None` picks one.
    pub bind: Option<String>,
}

impl Network {
    fn relay(&self) -> Result<RelayConfig> {
        Ok(match &self.relay {
            Relay::Default => RelayConfig::Default,
            Relay::Disabled => RelayConfig::Disabled,
            Relay::Custom { urls } => RelayConfig::Custom(
                urls.iter()
                    .map(|u| parse("relay URL", u))
                    .collect::<Result<_>>()?,
            ),
        })
    }

    fn bind(&self) -> Result<Option<SocketAddr>> {
        self.bind
            .as_deref()
            .map(|b| parse("bind address", b))
            .transpose()
    }
}

/// This device's ID, creating its key in `data_dir` on first use.
#[uniffi::export]
pub fn device_id(data_dir: String) -> Result<String> {
    Ok(device::device_key(&PathBuf::from(data_dir))?
        .id()
        .to_string())
}

/// True once the device in `data_dir` has a certificate.
#[uniffi::export]
pub fn is_paired(data_dir: String) -> Result<bool> {
    let dir = PathBuf::from(data_dir);
    let key = device::device_key(&dir)?;
    Ok(device::certificate(&dir, &key)?.is_some())
}

/// A folder offered by the approver at the end of pairing.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FolderOffer {
    /// Folder ID.
    pub id: String,
    /// Display name (the folder's name on the approver).
    pub name: String,
    /// User IDs allowed to sync it.
    pub members: Vec<String>,
    /// Recycle-bin retention in days; 0 keeps deleted files forever.
    pub retention_days: u32,
}

/// Progress of a [`Pairing`].
#[derive(Debug, Clone, uniffi::Enum)]
pub enum PairingUpdate {
    /// A device holding the user key found this one; show the confirmation code and ask the
    /// person to compare it with the one on that device.
    Confirming {
        /// The approving device's name.
        approver_name: String,
        /// The user it belongs to.
        user: String,
        /// Six digits.
        confirmation_code: String,
    },
    /// Paired. The certificate and the approver are stored; start a [`Node`].
    Paired {
        /// This device's name, as the approver certified it.
        name: String,
        /// The user this device now belongs to.
        user: String,
        /// Folders the person may join.
        folders: Vec<FolderOffer>,
    },
    /// One attempt failed; the code stays valid until it expires.
    Failed {
        /// Why.
        reason: String,
    },
    /// The code expired or pairing was cancelled.
    Stopped,
}

/// A new device waiting to be paired.
#[derive(uniffi::Object)]
pub struct Pairing {
    data_dir: PathBuf,
    code: String,
    server: Arc<tokio::sync::Mutex<Option<PairingServer>>>,
    cancelled: Arc<tokio::sync::Notify>,
}

#[uniffi::export]
impl Pairing {
    /// Starts a pairing-only endpoint for the device in `data_dir`, suggesting `name` for it.
    #[uniffi::constructor]
    pub async fn start(data_dir: String, network: Network, name: String) -> Result<Arc<Self>> {
        let data_dir = PathBuf::from(data_dir);
        let key = device::device_key(&data_dir)?;
        if device::certificate(&data_dir, &key)?.is_some() {
            return Err(invalid("this device is already paired"));
        }
        let (relay, bind) = (network.relay()?, network.bind()?);
        let lookup = network.address_lookup;
        let server =
            on_runtime(
                async move { pairing::start_pairing(&key, relay, lookup, bind, &name).await },
            )
            .await??;
        Ok(Arc::new(Self {
            data_dir,
            code: server.code().to_text(),
            server: Arc::new(tokio::sync::Mutex::new(Some(server))),
            cancelled: Arc::new(tokio::sync::Notify::new()),
        }))
    }

    /// The pairing code's text form (`TFP1-…`), for a QR code.
    pub fn code(&self) -> String {
        self.code.clone()
    }

    /// Waits for what happens next.
    pub async fn next_update(&self) -> Result<PairingUpdate> {
        let (server, data_dir) = (self.server.clone(), self.data_dir.clone());
        let cancelled = self.cancelled.clone();
        on_runtime(async move {
            let mut guard = server.lock().await;
            let Some(active) = guard.as_mut() else {
                return Ok(PairingUpdate::Stopped);
            };
            let event = tokio::select! {
                event = tokio::time::timeout(pairing::CODE_TTL, active.next_event()) => event,
                () = cancelled.notified() => Ok(None),
            };
            match event {
                Ok(Some(PairingEvent::Confirming {
                    approver,
                    confirmation_code,
                })) => Ok(PairingUpdate::Confirming {
                    approver_name: approver.name,
                    user: approver.user.to_string(),
                    confirmation_code,
                }),
                Ok(Some(PairingEvent::Failed { reason })) => Ok(PairingUpdate::Failed { reason }),
                Ok(Some(PairingEvent::Paired(paired))) => {
                    if let Some(server) = guard.take() {
                        server.shutdown().await;
                    }
                    device::save_certificate(&data_dir, &paired.certificate)?;
                    pairing::remember_approver(&data_dir, &paired)?;
                    let certificate = paired
                        .certificate
                        .verify_certificate()
                        .map_err(|e| invalid(e.to_string()))?;
                    Ok(PairingUpdate::Paired {
                        name: certificate.name,
                        user: certificate.user.to_string(),
                        folders: paired
                            .folders
                            .iter()
                            .map(|f| FolderOffer {
                                id: f.id.to_string(),
                                name: f.name.clone(),
                                members: f.members.iter().map(ToString::to_string).collect(),
                                retention_days: f.retention_days,
                            })
                            .collect(),
                    })
                }
                Ok(None) | Err(_) => {
                    if let Some(server) = guard.take() {
                        server.shutdown().await;
                    }
                    Ok(PairingUpdate::Stopped)
                }
            }
        })
        .await?
    }

    /// Stops pairing; a pending or later [`Pairing::next_update`] returns
    /// [`PairingUpdate::Stopped`].
    pub fn cancel(&self) {
        // Stores a permit if nothing is waiting, so the next update sees it.
        self.cancelled.notify_one();
    }
}

/// This device.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DeviceStatus {
    /// Device ID.
    pub device: String,
    /// The user it belongs to.
    pub user: String,
    /// Its name.
    pub name: String,
}

/// A synced folder.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Folder {
    /// Folder ID.
    pub id: String,
    /// Where its files are on this device.
    pub path: String,
    /// For a partial replica, the bytes of files it keeps on disk; `None` for a full replica.
    pub partial_budget: Option<u64>,
    /// User IDs allowed to sync it.
    pub members: Vec<String>,
    /// Recycle-bin retention in days; 0 keeps deleted files forever.
    pub retention_days: u32,
}

/// A live file.
#[derive(Debug, Clone, uniffi::Record)]
pub struct File {
    /// Stable entry ID.
    pub entry: String,
    /// Path within the folder, `/`-separated.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub modified_ms: i64,
    /// True if its current content is on this device; otherwise a placeholder.
    pub on_disk: bool,
    /// Unresolved conflict copies.
    pub conflicts: u32,
}

/// A deleted file in the recycle bin.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Recycled {
    /// Entry ID, to restore it.
    pub entry: String,
    /// Path it had.
    pub path: String,
    /// Size in bytes.
    pub size: u64,
    /// When it was deleted, milliseconds since the Unix epoch.
    pub deleted_at_ms: i64,
    /// When it will be purged; `None` if kept forever.
    pub expires_at_ms: Option<i64>,
}

/// A connected peer.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Peer {
    /// Device ID.
    pub device: String,
    /// Its user.
    pub user: String,
    /// Its name.
    pub name: String,
    /// Folders shared with it.
    pub shared_folders: Vec<String>,
}

/// Something changed.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum NodeEvent {
    /// A folder was added.
    FolderAdded {
        /// Folder ID.
        folder: String,
    },
    /// A folder was removed.
    FolderRemoved {
        /// Folder ID.
        folder: String,
    },
    /// A folder's files or membership changed.
    FolderChanged {
        /// Folder ID.
        folder: String,
    },
    /// A file was written to disk.
    FileUpdated {
        /// Folder ID.
        folder: String,
        /// File path.
        path: String,
    },
    /// A file was removed from disk (deleted, or evicted back to a placeholder).
    FileRemoved {
        /// Folder ID.
        folder: String,
        /// File path.
        path: String,
    },
}

/// Receives [`NodeEvent`]s.
#[uniffi::export(callback_interface)]
pub trait EventListener: Send + Sync {
    /// Called on a library thread for every event.
    fn on_event(&self, event: NodeEvent);
}

/// A running peer.
#[derive(uniffi::Object)]
pub struct Node {
    engine: RwLock<Option<Arc<Engine>>>,
    user: tidefell_engine::UserId,
}

impl Node {
    fn engine(&self) -> Result<Arc<Engine>> {
        self.engine
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| invalid("the node has shut down"))
    }
}

fn retention(days: u32) -> Retention {
    if days == 0 {
        Retention::Forever
    } else {
        Retention::Days(days)
    }
}

fn folder_record(f: tidefell_engine::FolderInfo) -> Folder {
    Folder {
        id: f.id.to_string(),
        path: f.path.display().to_string(),
        partial_budget: match f.mode {
            ReplicaMode::Full => None,
            ReplicaMode::Partial { budget_bytes } => Some(budget_bytes),
        },
        members: f.members.iter().map(ToString::to_string).collect(),
        retention_days: match f.retention {
            Retention::Forever => 0,
            Retention::Days(d) => d,
        },
    }
}

#[uniffi::export]
impl Node {
    /// Starts the engine for the paired device in `data_dir`. Folders joined earlier are
    /// resumed.
    #[uniffi::constructor]
    pub async fn start(data_dir: String, network: Network) -> Result<Arc<Self>> {
        let data_dir = PathBuf::from(data_dir);
        let key = device::device_key(&data_dir)?;
        let certificate = device::certificate(&data_dir, &key)?.ok_or(TidefellError::NotPaired)?;
        let user = certificate
            .verify_certificate()
            .map_err(|e| invalid(e.to_string()))?
            .user;
        let config = EngineConfig {
            data_dir,
            relay: network.relay()?,
            address_lookup: network.address_lookup,
            bind: network.bind()?,
            peers: Vec::new(),
            folders: Vec::new(),
        };
        let engine = on_runtime(Engine::start(config, key, certificate)).await??;
        Ok(Arc::new(Self {
            engine: RwLock::new(Some(Arc::new(engine))),
            user,
        }))
    }

    /// This device.
    pub fn status(&self) -> Result<DeviceStatus> {
        let engine = self.engine()?;
        let certificate = engine
            .certificate()
            .verify_certificate()
            .map_err(|e| invalid(e.to_string()))?;
        Ok(DeviceStatus {
            device: engine.id().to_string(),
            user: certificate.user.to_string(),
            name: certificate.name,
        })
    }

    /// All folders.
    pub fn folders(&self) -> Result<Vec<Folder>> {
        Ok(self
            .engine()?
            .folders()
            .into_iter()
            .map(folder_record)
            .collect())
    }

    /// Joins folder `id` (from a [`FolderOffer`] or another device) at `path`. With
    /// `partial_budget`, other devices' files are placeholders until opened. `members` defaults
    /// to this device's user.
    pub fn join_folder(
        &self,
        id: String,
        path: String,
        partial_budget: Option<u64>,
        members: Vec<String>,
        retention_days: u32,
    ) -> Result<Folder> {
        let id: FolderId = parse("folder ID", &id)?;
        self.add(id, path, partial_budget, members, retention_days)
    }

    /// Creates a new folder at `path`, shared with this device's user.
    pub fn create_folder(
        &self,
        path: String,
        partial_budget: Option<u64>,
        retention_days: u32,
    ) -> Result<Folder> {
        let mut bytes = [0u8; 32];
        getrandom_fill(&mut bytes)?;
        self.add(
            FolderId(bytes),
            path,
            partial_budget,
            Vec::new(),
            retention_days,
        )
    }

    /// Stops syncing a folder. Files on disk are kept.
    pub fn remove_folder(&self, id: String) -> Result<()> {
        Ok(self.engine()?.remove_folder(parse("folder ID", &id)?)?)
    }

    /// Live files in a folder, sorted by path.
    pub fn files(&self, folder: String) -> Result<Vec<File>> {
        Ok(self
            .engine()?
            .files(parse("folder ID", &folder)?)?
            .into_iter()
            .map(|f| File {
                entry: f.entry.to_string(),
                path: f.path,
                size: f.size,
                modified_ms: f.modified_ms,
                on_disk: f.on_disk,
                conflicts: f.conflicts as u32,
            })
            .collect())
    }

    /// Makes a file available on disk, fetching a placeholder from peers, and returns its
    /// path. It stays pinned until [`Node::close`].
    pub async fn open(&self, folder: String, path: String) -> Result<String> {
        let engine = self.engine()?;
        let folder: FolderId = parse("folder ID", &folder)?;
        let local = on_runtime(async move { engine.open(folder, &path).await }).await??;
        Ok(local.display().to_string())
    }

    /// Releases one [`Node::open`].
    pub fn close(&self, folder: String, path: String) -> Result<()> {
        Ok(self.engine()?.close(parse("folder ID", &folder)?, &path)?)
    }

    /// Moves the file at `source` into the folder at `path` and records the change.
    pub async fn write(&self, folder: String, path: String, source: String) -> Result<()> {
        let engine = self.engine()?;
        let folder: FolderId = parse("folder ID", &folder)?;
        on_runtime(async move { engine.write(folder, &path, &PathBuf::from(source)).await })
            .await??;
        Ok(())
    }

    /// Scans a folder for local changes now.
    pub async fn scan(&self, folder: String) -> Result<()> {
        let engine = self.engine()?;
        let folder: FolderId = parse("folder ID", &folder)?;
        on_runtime(async move { engine.scan(folder).await }).await??;
        Ok(())
    }

    /// Deleted files, most recent first.
    pub fn recycle_bin(&self, folder: String) -> Result<Vec<Recycled>> {
        Ok(self
            .engine()?
            .recycle_bin(parse("folder ID", &folder)?)?
            .into_iter()
            .map(|r| Recycled {
                entry: r.entry.to_string(),
                path: r.path,
                size: r.size,
                deleted_at_ms: r.deleted_at_ms,
                expires_at_ms: r.expires_at_ms,
            })
            .collect())
    }

    /// Restores a deleted file on every device.
    pub fn restore(&self, folder: String, entry: String) -> Result<()> {
        Ok(self
            .engine()?
            .restore(parse("folder ID", &folder)?, parse("entry ID", &entry)?)?)
    }

    /// Connected peers.
    pub fn peers(&self) -> Result<Vec<Peer>> {
        Ok(self
            .engine()?
            .connected_peers()
            .into_iter()
            .map(|p| Peer {
                device: p.device.to_string(),
                user: p.user.to_string(),
                name: p.name,
                shared_folders: p.shared_folders.iter().map(ToString::to_string).collect(),
            })
            .collect())
    }

    /// Keeps a peer connected, with optional direct addresses (`host:port`).
    pub fn add_peer(&self, device: String, addrs: Vec<String>) -> Result<()> {
        Ok(self.engine()?.add_peer(PeerAddr {
            device: parse("device ID", &device)?,
            addrs: addrs
                .iter()
                .map(|a| parse("address", a))
                .collect::<Result<_>>()?,
        })?)
    }

    /// Calls `listener` for every event until the node shuts down.
    pub fn subscribe(&self, listener: Box<dyn EventListener>) -> Result<()> {
        let mut events = self.engine()?.events();
        runtime().spawn(async move {
            use tidefell_engine::Event as E;
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                listener.on_event(match event {
                    E::FolderAdded(id) => NodeEvent::FolderAdded {
                        folder: id.to_string(),
                    },
                    E::FolderRemoved(id) => NodeEvent::FolderRemoved {
                        folder: id.to_string(),
                    },
                    E::FolderChanged(id) => NodeEvent::FolderChanged {
                        folder: id.to_string(),
                    },
                    E::FileUpdated { folder, path } => NodeEvent::FileUpdated {
                        folder: folder.to_string(),
                        path,
                    },
                    E::FileRemoved { folder, path } => NodeEvent::FileRemoved {
                        folder: folder.to_string(),
                        path,
                    },
                });
            }
        });
        Ok(())
    }

    /// Stops the engine and saves its state.
    pub async fn shutdown(&self) -> Result<()> {
        let Some(mut engine) = self.engine.write().unwrap().take() else {
            return Ok(());
        };
        on_runtime(async move {
            // Calls still in flight hold the engine briefly.
            for _ in 0..50 {
                match Arc::try_unwrap(engine) {
                    Ok(engine) => return engine.shutdown().await,
                    Err(shared) => {
                        engine = shared;
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        })
        .await
    }
}

impl Node {
    fn add(
        &self,
        id: FolderId,
        path: String,
        partial_budget: Option<u64>,
        members: Vec<String>,
        retention_days: u32,
    ) -> Result<Folder> {
        let engine = self.engine()?;
        let members = if members.is_empty() {
            vec![self.user]
        } else {
            members
                .iter()
                .map(|m| parse("user ID", m))
                .collect::<Result<_>>()?
        };
        engine.add_folder(FolderConfig {
            id,
            path: PathBuf::from(path),
            mode: match partial_budget {
                None => ReplicaMode::Full,
                Some(budget_bytes) => ReplicaMode::Partial { budget_bytes },
            },
            members,
            retention: retention(retention_days),
            scan_interval: Duration::from_secs(30),
        })?;
        engine
            .folders()
            .into_iter()
            .find(|f| f.id == id)
            .map(folder_record)
            .ok_or_else(|| TidefellError::NotFound {
                message: format!("folder {id}"),
            })
    }
}

fn getrandom_fill(bytes: &mut [u8]) -> Result<()> {
    getrandom::fill(bytes).map_err(|e| TidefellError::Failed {
        message: e.to_string(),
    })
}
