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

#[global_allocator]
static ALLOCATOR: tidefell_engine::Allocator = tidefell_engine::Allocator;

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use tidefell_engine::diagnostics;
use tidefell_engine::pairing::{self, PairingEvent, PairingServer};
use tidefell_engine::{
    device, Engine, EngineConfig, FolderId, NewFolder, PeerAddr, RelayConfig, RelayToken,
    ReplicaMode, Retention,
};

uniffi::setup_scaffolding!();

mod vault;
pub use vault::{set_key_vault, KeyVault};

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
            detail: e.to_string(),
        })
}

/// Errors reported to the app.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum TidefellError {
    /// A folder, file or entry does not exist.
    #[error("{detail}")]
    NotFound {
        /// What is missing.
        detail: String,
    },
    /// Content or a peer cannot be reached right now.
    #[error("{detail}")]
    Unavailable {
        /// What is unavailable.
        detail: String,
    },
    /// The request was invalid.
    #[error("{detail}")]
    Invalid {
        /// Why.
        detail: String,
    },
    /// The device has no certificate yet; pair it first.
    #[error("this device is not paired yet")]
    NotPaired,
    /// Anything else.
    #[error("{detail}")]
    Failed {
        /// What failed.
        detail: String,
    },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for TidefellError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed { detail: e.reason }
    }
}

impl From<tidefell_engine::Error> for TidefellError {
    fn from(e: tidefell_engine::Error) -> Self {
        use tidefell_engine::Error as E;
        let detail = e.to_string();
        match e {
            E::UnknownFolder(_) | E::NoSuchFile(_) | E::NotRecycled(_) => Self::NotFound { detail },
            E::Unavailable(_) | E::Net(_) | E::Paused(_) => Self::Unavailable { detail },
            E::Invalid(_)
            | E::FolderExists(_)
            | E::Configured(_)
            | E::LocalChanges(_)
            | E::PathTaken(_) => Self::Invalid { detail },
            _ => Self::Failed { detail },
        }
    }
}

type Result<T> = std::result::Result<T, TidefellError>;

fn invalid(message: impl Into<String>) -> TidefellError {
    TidefellError::Invalid {
        detail: message.into(),
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
        /// The token the relays want, for relays that admit only holders of it
        /// (`access.shared_token` in iroh-relay); none for relays that admit anyone.
        token: Option<String>,
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
    /// Announce this device and find known devices on the local network with mDNS.
    pub local_discovery: bool,
    /// Local socket to listen on, e.g. `0.0.0.0:0`; `None` picks one.
    pub bind: Option<String>,
}

impl Network {
    fn relay(&self) -> Result<RelayConfig> {
        Ok(match &self.relay {
            Relay::Default => RelayConfig::Default,
            Relay::Disabled => RelayConfig::Disabled,
            Relay::Custom { urls, token } => RelayConfig::Custom {
                urls: urls
                    .iter()
                    .map(|u| parse("relay URL", u))
                    .collect::<Result<_>>()?,
                token: token
                    .as_deref()
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(|t| {
                        RelayToken::new(t)
                            .ok_or_else(|| invalid("a relay token is visible ASCII only"))
                    })
                    .transpose()?,
            },
        })
    }

    fn bind(&self) -> Result<Option<SocketAddr>> {
        self.bind
            .as_deref()
            .map(|b| parse("bind address", b))
            .transpose()
    }
}

/// Starts logging and crash recording (ADR 0030): JSON lines in `<data_dir>/logs/`, logcat on
/// Android and standard error, at `level` (such as `info` or `info,tidefell_net=debug`), and a
/// crash file in `<data_dir>/crashes/` for every panic. Call it once, before anything else. Only
/// the first call has an effect; it returns false for later ones.
#[uniffi::export]
pub fn init_diagnostics(data_dir: String, level: String) -> Result<bool> {
    let data_dir = PathBuf::from(data_dir);
    diagnostics::install_panic_hook(&data_dir);
    diagnostics::init_logging(&diagnostics::LogOptions {
        data_dir,
        level,
        file: Some(true),
        stderr: true,
    })
    .map_err(|e| invalid(e.to_string()))
}

/// Changes the log level while running.
#[uniffi::export]
pub fn set_log_level(level: String) -> Result<()> {
    diagnostics::set_log_level(&level).map_err(|e| invalid(e.to_string()))
}

/// The current log level, or none before [`init_logging`].
#[uniffi::export]
pub fn log_level() -> Option<String> {
    diagnostics::log_level()
}

/// How severe a message from the app is.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum LogLevel {
    /// Detail for debugging.
    Debug,
    /// Normal operation.
    Info,
    /// Something went wrong but syncing carries on.
    Warn,
    /// Something failed.
    Error,
}

/// Logs one of the app's own messages, so it lands in the same file as the engine's.
#[uniffi::export]
pub fn log(level: LogLevel, tag: String, message: String) {
    match level {
        LogLevel::Debug => tracing::debug!(target: "app", tag, "{message}"),
        LogLevel::Info => tracing::info!(target: "app", tag, "{message}"),
        LogLevel::Warn => tracing::warn!(target: "app", tag, "{message}"),
        LogLevel::Error => tracing::error!(target: "app", tag, "{message}"),
    }
}

/// A recent warning or error.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Problem {
    /// When, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// `WARN` or `ERROR`.
    pub level: String,
    /// Where it was logged.
    pub target: String,
    /// The message.
    pub message: String,
    /// Its structured fields, such as the folder and path.
    pub fields: HashMap<String, String>,
}

/// Upload and download limits for file content, in bytes per second (ADR 0029).
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct Limits {
    /// Most bytes per second sent; none for no limit. At least 64 KiB/s.
    pub upload: Option<u64>,
    /// Most bytes per second received; none for no limit. At least 64 KiB/s.
    pub download: Option<u64>,
    /// Also limit traffic to devices on the local network.
    pub limit_local: bool,
}

/// A crash recorded in `<data_dir>/crashes/`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Crash {
    /// Names it for [`crash_report`]: its file's name.
    pub id: String,
    /// The crash file.
    pub file: String,
    /// When, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// What went wrong.
    pub message: String,
    /// Whether the user has already been told.
    pub seen: bool,
}

/// A crash written up as a new issue for the project's tracker (ADR 0030). Nothing is sent:
/// the user reviews it and files it.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CrashReport {
    /// The issue's title.
    pub title: String,
    /// Its body, in Markdown, with home directories replaced by `~`.
    pub body: String,
    /// A link that opens a new issue pre-filled with them, if a tracker is known; its body may be
    /// cut to fit a link. Without one, the app offers the title and body to copy.
    pub url: Option<String>,
}

/// The crash `id` in `data_dir` written up for `report_url`, the tracker's issues page from the
/// app's settings, or the default tracker if `None` (ADR 0030).
#[uniffi::export]
pub fn crash_report(
    data_dir: String,
    id: String,
    report_url: Option<String>,
) -> Result<CrashReport> {
    if let Some(url) = &report_url {
        diagnostics::check_report_url(url).map_err(invalid)?;
    }
    let url = report_url.or(diagnostics::DEFAULT_REPORT_URL.map(str::to_owned));
    let report = diagnostics::crash_report(&PathBuf::from(data_dir), &id, url.as_deref())
        .ok_or(TidefellError::NotFound { detail: id })?;
    Ok(CrashReport {
        title: report.title,
        body: report.body,
        url: report.url,
    })
}

/// Crashes recorded for the device in `data_dir`, oldest first.
#[uniffi::export]
pub fn crashes(data_dir: String) -> Vec<Crash> {
    diagnostics::crashes(&PathBuf::from(data_dir))
        .into_iter()
        .map(|c| Crash {
            id: c.id,
            file: c.file.display().to_string(),
            time_ms: c.time_ms,
            message: c.message,
            seen: c.seen,
        })
        .collect()
}

/// Marks every crash recorded so far as seen.
#[uniffi::export]
pub fn dismiss_crashes(data_dir: String) -> Result<()> {
    diagnostics::dismiss_crashes(&PathBuf::from(data_dir)).map_err(|e| invalid(e.to_string()))
}

/// Records one of the app's own crashes, such as an uncaught exception, in the same place and
/// format as the engine's. `details` is the stack trace.
#[uniffi::export]
pub fn record_crash(data_dir: String, message: String, details: String) -> Result<()> {
    let dir = PathBuf::from(data_dir).join("crashes");
    diagnostics::write_crash(&dir, "app", &message, &details)
        .map(drop)
        .map_err(|e| invalid(e.to_string()))
}

/// The most recent warnings and errors, oldest first.
#[uniffi::export]
pub fn recent_problems() -> Vec<Problem> {
    diagnostics::recent_problems()
        .into_iter()
        .map(|p| Problem {
            time_ms: p.time_ms,
            level: p.level,
            target: p.target,
            message: p.message,
            fields: p.fields.into_iter().collect(),
        })
        .collect()
}

/// This device's ID, creating its key in `data_dir` on first use.
#[uniffi::export]
pub fn device_id(data_dir: String) -> Result<String> {
    Ok(vault::device_key(&PathBuf::from(data_dir))?
        .id()
        .to_string())
}

/// True once the device in `data_dir` has a certificate.
#[uniffi::export]
pub fn is_paired(data_dir: String) -> Result<bool> {
    let dir = PathBuf::from(data_dir);
    let key = vault::device_key(&dir)?;
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
    /// For an encrypted folder, what to join it with: pass it on to [`Node::join_folder`].
    pub encryption: Option<Encryption>,
}

/// An encrypted folder's settings when it is created or joined (ADR 0023).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Encryption {
    /// Device IDs of storage peers: devices that hold the folder without reading it.
    pub storage_peers: Vec<String>,
}

/// How a folder is encrypted on this device (ADR 0023).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FolderEncryption {
    /// Device IDs of its storage peers.
    pub storage_peers: Vec<String>,
    /// The generation of the folder key new content is sealed with; `None` until a key has
    /// reached this device from another member device.
    pub key_generation: Option<u64>,
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
        let key = vault::device_key(&data_dir)?;
        if device::certificate(&data_dir, &key)?.is_some() {
            return Err(invalid("this device is already paired"));
        }
        let (relay, bind) = (network.relay()?, network.bind()?);
        let (lookup, local) = (network.address_lookup, network.local_discovery);
        let server = on_runtime(async move {
            pairing::start_pairing(&key, relay, lookup, local, bind, &name).await
        })
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
                                encryption: f.encryption.as_ref().map(|e| Encryption {
                                    storage_peers: e
                                        .storage_peers
                                        .iter()
                                        .map(ToString::to_string)
                                        .collect(),
                                }),
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

/// A revoked device's changes after its loss, waiting for a decision (ADR 0040).
#[derive(Debug, Clone, uniffi::Record)]
pub struct HeldWithdrawals {
    /// The revoked device.
    pub device: String,
    /// The files withdrawing its changes would remove.
    pub paths: Vec<String>,
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
    /// Syncing is paused for the whole device (ADR 0029).
    pub paused: bool,
    /// Its user revoked it (ADR 0040): it syncs with no one, and has to be paired again.
    pub revoked: bool,
}

/// Whether other devices' changes are written to this device (ADR 0025).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Direction {
    /// Send local changes and write other devices' changes here.
    Both,
    /// Send local changes only; other devices' changes are never written here.
    SendOnly,
}

/// What deleting a file on this device, outside the library, does (ADR 0025).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LocalDeletes {
    /// The delete is synced: the file moves to every device's recycle bin.
    Everywhere,
    /// The file is dropped from this device only; it stays on the others and is not downloaded
    /// again until opened.
    ThisDevice,
}

/// How a folder syncs on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct SyncOptions {
    /// Whether other devices' changes are written here.
    pub direction: Direction,
    /// What deleting a file here does.
    pub local_deletes: LocalDeletes,
}

impl From<SyncOptions> for tidefell_engine::SyncOptions {
    fn from(s: SyncOptions) -> Self {
        Self {
            direction: match s.direction {
                Direction::Both => tidefell_engine::Direction::Both,
                Direction::SendOnly => tidefell_engine::Direction::SendOnly,
            },
            local_deletes: match s.local_deletes {
                LocalDeletes::Everywhere => tidefell_engine::LocalDeletes::Everywhere,
                LocalDeletes::ThisDevice => tidefell_engine::LocalDeletes::ThisDevice,
            },
        }
    }
}

impl From<tidefell_engine::SyncOptions> for SyncOptions {
    fn from(s: tidefell_engine::SyncOptions) -> Self {
        Self {
            direction: match s.direction {
                tidefell_engine::Direction::Both => Direction::Both,
                tidefell_engine::Direction::SendOnly => Direction::SendOnly,
            },
            local_deletes: match s.local_deletes {
                tidefell_engine::LocalDeletes::Everywhere => LocalDeletes::Everywhere,
                tidefell_engine::LocalDeletes::ThisDevice => LocalDeletes::ThisDevice,
            },
        }
    }
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
    /// How it syncs on this device.
    pub sync: SyncOptions,
    /// Deleted files in this device's recycle bin, including files removed from this device
    /// only.
    pub recycled_files: u64,
    /// Their total size in bytes.
    pub recycled_bytes: u64,
    /// When the next one will be purged, milliseconds since the Unix epoch; `None` if none
    /// will be.
    pub next_purge_ms: Option<i64>,
    /// Live files made on this device that no full replica has confirmed holding yet
    /// (ADR 0022).
    pub only_here: u64,
    /// Syncing of this folder is paused (ADR 0029).
    pub paused: bool,
    /// Total size of the files pinned on this partial replica (ADR 0029).
    pub pinned_bytes: u64,
    /// Files with versions that conflict with the current one, waiting for a decision.
    pub conflicts: u64,
    /// What the folder is doing (ADR 0030).
    pub state: FolderState,
    /// Why the folder is in [`FolderState::Error`].
    pub error: Option<String>,
    /// Files this device is to write and does not have yet.
    pub pending_files: u64,
    /// Their total size.
    pub pending_bytes: u64,
    /// When the last scan finished, in milliseconds since the Unix epoch.
    pub last_scan_ms: Option<i64>,
    /// The peers the folder is shared with now or was exchanged with since the node started.
    pub peers: Vec<FolderPeer>,
    /// Paths that failed to scan, fetch, write or delete; see [`Node::path_problems`].
    pub problems: u64,
    /// The folder's last rebuild, if its metadata was ever started over (ADR 0034).
    pub rebuild: Option<Rebuild>,
    /// Why the folder's root cannot be used, if it cannot, as when the SD card holding it is not
    /// mounted: nothing is scanned, written or deleted there until it is back (ADR 0037).
    pub unavailable: Option<String>,
    /// Files the last scan found deleted, so many that the deletes wait for
    /// [`Node::confirm_deletes`] or [`Node::restore_deleted`] (ADR 0037).
    pub held_deletes: u64,
    /// Why nothing more is downloaded to this folder, if the phone is low on space (ADR 0037).
    pub low_space: Option<String>,
    /// For an encrypted folder, how (ADR 0023).
    pub encryption: Option<FolderEncryption>,
}

/// A folder's rebuild (ADR 0034): its metadata was started over, from the files on disk and
/// what peers send.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Rebuild {
    /// When it began, in milliseconds since the Unix epoch.
    pub started_ms: i64,
    /// Why, in words.
    pub reason: String,
    /// When it finished, if it has.
    pub finished_ms: Option<i64>,
    /// While it has not: the connected devices sharing the folder whose index it waits for.
    pub waiting_for: Vec<String>,
}

/// What a folder is doing (ADR 0030).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FolderState {
    /// Nothing to do.
    Idle,
    /// Looking for local changes.
    Scanning,
    /// Files are pending and a connected peer shares the folder.
    Syncing,
    /// Paused.
    Paused,
    /// Its metadata is being started over, and it waits for a peer's index (ADR 0034).
    Rebuilding,
    /// The folder cannot be used, or its last scan failed.
    Error,
}

/// A folder's exchanges with one peer.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FolderPeer {
    /// Device ID.
    pub device: String,
    /// When the folder last finished exchanging changes with it, since the node started.
    pub last_exchange_ms: Option<i64>,
    /// Connected and caught up.
    pub up_to_date: bool,
}

/// A device this one syncs with, connected or not (ADR 0030).
#[derive(Debug, Clone, uniffi::Record)]
pub struct PeerState {
    /// Device ID.
    pub device: String,
    /// Its name, if it has connected since the node started.
    pub name: Option<String>,
    /// A sync session is running.
    pub connected: bool,
    /// Through a relay rather than directly, while connected.
    pub relayed: Option<bool>,
    /// Round-trip time in milliseconds, while connected.
    pub rtt_ms: Option<u64>,
    /// When a session was last up: now while connected.
    pub last_seen_ms: Option<i64>,
    /// Why the last attempt to connect, or the last session, failed.
    pub last_error: Option<String>,
    /// How far its clock was ahead of this device's at the last handshake, in milliseconds.
    pub clock_offset_ms: Option<i64>,
}

/// A diagnostics bundle written for sharing (ADR 0030).
#[derive(Debug, Clone, uniffi::Record)]
pub struct DiagnosticsBundle {
    /// The zip file.
    pub path: String,
    /// What it holds, to show before sharing.
    pub entries: Vec<DiagnosticsEntry>,
}

/// One file of a diagnostics bundle.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DiagnosticsEntry {
    /// Its name in the zip file.
    pub name: String,
    /// Its content, already redacted.
    pub text: String,
}

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum CheckLevel {
    /// Fine.
    Pass,
    /// Works, but worth attention.
    Warning,
    /// Something does not work.
    Failure,
}

/// One check of [`Node::doctor`] (ADR 0030).
#[derive(Debug, Clone, uniffi::Record)]
pub struct Check {
    /// What was checked: `state`, `clock`, `space`, `folder`, `filesystem`, `watch`, `relay`,
    /// `network` or `peers`.
    pub name: String,
    /// How it came out.
    pub level: CheckLevel,
    /// What was found.
    pub detail: String,
    /// What to do about it, for a warning or failure.
    pub advice: Option<String>,
}

/// File content moved since the node started, in bytes and bytes per second.
#[derive(Debug, Clone, Copy, uniffi::Record)]
pub struct Transfer {
    /// Bytes sent.
    pub sent: u64,
    /// Bytes received.
    pub received: u64,
    /// Bytes per second sent, over the last ten seconds.
    pub send_rate: u64,
    /// Bytes per second received, over the last ten seconds.
    pub receive_rate: u64,
}

/// What a failing path was being used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Operation {
    /// Reading it for changes.
    Scan,
    /// Fetching its content from other devices.
    Fetch,
    /// Writing it to disk.
    Write,
    /// Deleting it from disk.
    Delete,
}

/// Why a path failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ProblemKind {
    /// Not allowed, including a read-only disk.
    PermissionDenied,
    /// No space left.
    DiskFull,
    /// No connected device has the content.
    ContentUnavailable,
    /// A name this device cannot sync.
    NameNotAllowed,
    /// The file changed while it was read or written; tried again later.
    ChangedWhileWriting,
    /// Anything else; see the message.
    Other,
}

/// What a change in a folder's activity log did (ADR 0030).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ChangeKind {
    /// A new file.
    Added,
    /// New content.
    Changed,
    /// Deleted.
    Deleted,
    /// Moved to another path.
    Renamed,
    /// Live again after it was deleted.
    Restored,
    /// A version made apart from the current one arrived, and waits for a decision.
    Conflict,
}

/// One change this device applied to a folder (ADR 0030).
#[derive(Debug, Clone, uniffi::Record)]
pub struct Activity {
    /// When this device applied it, in milliseconds since the Unix epoch.
    pub time_ms: i64,
    /// What happened.
    pub kind: ChangeKind,
    /// The file's path.
    pub path: String,
    /// Its path before, for a rename.
    pub old_path: Option<String>,
    /// The device that made the change.
    pub device: String,
    /// That device's name, as far as this device knows it, or the start of its ID.
    pub device_name: String,
    /// Made on this device, rather than received from a peer.
    pub local: bool,
}

/// A path that failed, until it succeeds (ADR 0030).
#[derive(Debug, Clone, uniffi::Record)]
pub struct PathProblem {
    /// Path within the folder.
    pub path: String,
    /// What it was being used for.
    pub operation: Operation,
    /// Why it failed.
    pub kind: ProblemKind,
    /// The error.
    pub message: String,
    /// When it first failed.
    pub since_ms: i64,
    /// How many passes in a row it has failed.
    pub attempts: u32,
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
    /// Full replicas that confirmed holding it (ADR 0022). Only tracked for files made on this
    /// device; 0 otherwise.
    pub confirmed_copies: u32,
    /// True if this device made it and no full replica has confirmed holding it yet, so this
    /// device may hold the only copy.
    pub only_here: bool,
    /// Pinned on this device (ADR 0029): kept on disk and never evicted.
    pub pinned: bool,
    /// The device that made the current version.
    pub author: String,
    /// That device's name, as far as this device knows it, or the start of its ID (ADR 0030).
    pub author_name: String,
}

/// A full replica that confirmed holding a file.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Copy {
    /// Its device ID.
    pub device: String,
    /// Its name when it confirmed.
    pub name: String,
    /// When it confirmed, milliseconds since the Unix epoch.
    pub confirmed_ms: i64,
}

/// Which kind of version a [`Version`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum VersionKind {
    /// The version the folder has now.
    Current,
    /// A version kept on this device after it was replaced (ADR 0029).
    Previous,
    /// A version made apart from the current one, waiting for a decision (ADR 0029).
    Conflict,
}

/// One version of a file.
#[derive(Debug, Clone, uniffi::Record)]
pub struct Version {
    /// Identifies the version: `current`, or `previous:<id>`.
    pub id: String,
    /// Which kind it is.
    pub kind: VersionKind,
    /// Size in bytes.
    pub size: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub modified_ms: i64,
    /// For the current version, the device that made it.
    pub author: Option<String>,
    /// That device's name, as far as this device knows it.
    pub author_name: Option<String>,
    /// For a previous version, when this device kept it.
    pub kept_ms: Option<i64>,
    /// For a previous version, when it expires; none if kept for good.
    pub expires_ms: Option<i64>,
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
    /// True if it was removed from this device only: other devices still have it, and
    /// restoring it puts it back here only.
    pub here_only: bool,
    /// True if this is a version that lost when a conflict was resolved (ADR 0029). Restoring
    /// it makes it a file of its own beside `path`.
    pub conflict: bool,
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
    /// A file gained a version that conflicts with its current one (ADR 0029). The app shows a
    /// notification that opens the file's conflict screen.
    ConflictDetected {
        /// Folder ID.
        folder: String,
        /// File path.
        path: String,
    },
    /// A task panicked and the node stopped (ADR 0030). Start it again to resume syncing.
    Crashed {
        /// What went wrong.
        message: String,
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

fn folder_record(f: tidefell_engine::FolderInfo) -> Folder {
    Folder {
        id: f.id.to_string(),
        path: f.path.display().to_string(),
        partial_budget: f.mode.budget_bytes(),
        members: f.members.iter().map(ToString::to_string).collect(),
        retention_days: f.retention.days().unwrap_or(0),
        sync: f.sync.into(),
        recycled_files: f.recycle_bin.items as u64,
        recycled_bytes: f.recycle_bin.bytes,
        next_purge_ms: f.recycle_bin.next_expiry_ms,
        only_here: f.only_here as u64,
        paused: f.paused,
        pinned_bytes: f.pinned_bytes,
        conflicts: f.conflicts as u64,
        state: match f.state {
            tidefell_engine::FolderState::Idle => FolderState::Idle,
            tidefell_engine::FolderState::Scanning => FolderState::Scanning,
            tidefell_engine::FolderState::Syncing => FolderState::Syncing,
            tidefell_engine::FolderState::Paused => FolderState::Paused,
            tidefell_engine::FolderState::Rebuilding => FolderState::Rebuilding,
            tidefell_engine::FolderState::Error => FolderState::Error,
        },
        error: f.error,
        pending_files: f.pending_files as u64,
        pending_bytes: f.pending_bytes,
        last_scan_ms: f.last_scan_ms,
        peers: f
            .peers
            .into_iter()
            .map(|p| FolderPeer {
                device: p.device.to_string(),
                last_exchange_ms: p.last_exchange_ms,
                up_to_date: p.up_to_date,
            })
            .collect(),
        problems: f.problems as u64,
        rebuild: f.rebuild.map(|r| Rebuild {
            started_ms: r.started_ms,
            reason: r.reason,
            finished_ms: r.finished_ms,
            waiting_for: r.waiting_for.iter().map(ToString::to_string).collect(),
        }),
        unavailable: f.unavailable,
        held_deletes: f.held_deletes as u64,
        low_space: f.low_space,
        encryption: f.encryption.map(|e| FolderEncryption {
            storage_peers: e.storage_peers.iter().map(ToString::to_string).collect(),
            key_generation: e.key_generation,
        }),
    }
}

fn recycled_record(r: tidefell_engine::RecycledInfo) -> Recycled {
    Recycled {
        entry: r.entry.to_string(),
        path: r.path,
        size: r.size,
        deleted_at_ms: r.deleted_at_ms,
        expires_at_ms: r.expires_at_ms,
        here_only: r.here_only,
        conflict: r.conflict,
    }
}

#[uniffi::export]
impl Node {
    /// Starts the engine for the paired device in `data_dir`. Folders joined earlier are
    /// resumed.
    #[uniffi::constructor]
    pub async fn start(data_dir: String, network: Network) -> Result<Arc<Self>> {
        let data_dir = PathBuf::from(data_dir);
        let key = vault::device_key(&data_dir)?;
        let certificate = device::certificate(&data_dir, &key)?.ok_or(TidefellError::NotPaired)?;
        let config = EngineConfig {
            data_dir,
            metadata_dir: None,
            relay: network.relay()?,
            address_lookup: network.address_lookup,
            local_discovery: network.local_discovery,
            // The app triggers its own scans (ADR 0025).
            watch: false,
            limits: Default::default(),
            bind: network.bind()?,
            peers: Vec::new(),
            folders: Vec::new(),
            min_free_space: None,
        };
        let engine = Arc::new(on_runtime(Engine::start(config, key, certificate)).await??);
        let node = Arc::new(Self {
            engine: RwLock::new(Some(engine.clone())),
        });
        // A panic in an engine task stops the node: its state in memory may be inconsistent. The
        // app is told through the `Crashed` event and starts a new node.
        let (weak, failed) = (Arc::downgrade(&node), engine.failed());
        drop(engine);
        runtime().spawn(async move {
            let Some(message) = failed.await else {
                return;
            };
            if let Some(node) = weak.upgrade() {
                node.engine.write().unwrap().take();
                tracing::error!("the node stopped: {message}");
            }
        });
        Ok(node)
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
            paused: engine.paused(),
            revoked: engine.revoked(),
        })
    }

    /// Pauses syncing of `folder`, or of the whole device if none. It stays paused across
    /// restarts; folders still scan, but nothing is sent or received.
    pub fn pause(&self, folder: Option<String>) -> Result<()> {
        let folder = folder.map(|f| parse("folder ID", &f)).transpose()?;
        Ok(self.engine()?.pause(folder)?)
    }

    /// Resumes syncing of `folder`, or of the whole device if none.
    pub fn resume(&self, folder: Option<String>) -> Result<()> {
        let folder = folder.map(|f| parse("folder ID", &f)).transpose()?;
        Ok(self.engine()?.resume(folder)?)
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
    /// to this device's user. An encrypted folder is joined with `encryption`, which an offer
    /// carries (ADR 0023).
    #[expect(
        clippy::too_many_arguments,
        reason = "Kotlin calls it with named arguments, each a folder setting"
    )]
    pub fn join_folder(
        &self,
        id: String,
        path: String,
        partial_budget: Option<u64>,
        members: Vec<String>,
        retention_days: u32,
        sync: SyncOptions,
        encryption: Option<Encryption>,
    ) -> Result<Folder> {
        let id: FolderId = parse("folder ID", &id)?;
        self.add(
            Some(id),
            path,
            partial_budget,
            members,
            retention_days,
            sync,
            encryption,
        )
    }

    /// Creates a new folder at `path`, shared with this device's user; encrypted with
    /// `encryption` (ADR 0023), so storage peers can hold it without reading it.
    pub fn create_folder(
        &self,
        path: String,
        partial_budget: Option<u64>,
        retention_days: u32,
        sync: SyncOptions,
        encryption: Option<Encryption>,
    ) -> Result<Folder> {
        self.add(
            None,
            path,
            partial_budget,
            Vec::new(),
            retention_days,
            sync,
            encryption,
        )
    }

    /// Changes how a folder syncs on this device (ADR 0025). Files already dropped from this
    /// device stay dropped.
    pub fn set_sync_options(&self, folder: String, sync: SyncOptions) -> Result<Folder> {
        let engine = self.engine()?;
        let id: FolderId = parse("folder ID", &folder)?;
        engine.set_sync_options(id, sync.into())?;
        engine
            .folders()
            .into_iter()
            .find(|f| f.id == id)
            .map(folder_record)
            .ok_or(TidefellError::NotFound { detail: folder })
    }

    /// Changes how long this device keeps a folder's deleted files; 0 keeps them forever.
    /// Files past a shortened retention are purged within the hour, not immediately.
    pub fn set_retention(&self, folder: String, retention_days: u32) -> Result<Folder> {
        let engine = self.engine()?;
        let id: FolderId = parse("folder ID", &folder)?;
        engine.set_retention(id, Retention::from_days(retention_days))?;
        engine
            .folders()
            .into_iter()
            .find(|f| f.id == id)
            .map(folder_record)
            .ok_or(TidefellError::NotFound { detail: folder })
    }

    /// Stops syncing a folder. Files on disk are kept.
    pub fn remove_folder(&self, id: String) -> Result<()> {
        Ok(self.engine()?.remove_folder(parse("folder ID", &id)?)?)
    }

    /// Starts a folder's metadata over (ADR 0034): files on disk are matched to what peers
    /// send, and the recycle bin and history are restored from what was kept.
    pub async fn rebuild_folder(&self, id: String) -> Result<()> {
        let engine = self.engine()?;
        let folder = parse("folder ID", &id)?;
        on_runtime(async move { engine.rebuild_folder(folder).await }).await??;
        Ok(())
    }

    /// Tries again a folder's writes and deletes that failed in a way that lasts, such as
    /// permission refused, without waiting for the files to change (ADR 0037).
    pub fn retry_problems(&self, folder: String) -> Result<()> {
        Ok(self.engine()?.retry(parse("folder ID", &folder)?)?)
    }

    /// What revoked devices changed in `folder` after they were lost, waiting for a decision
    /// because there was so much (ADR 0040): for each, the device and the files withdrawing it
    /// would remove.
    pub fn held_withdrawals(&self, folder: String) -> Result<Vec<HeldWithdrawals>> {
        let folder = parse("folder ID", &folder)?;
        Ok(self
            .engine()?
            .held_withdrawals(folder)
            .into_iter()
            .map(|(device, paths)| HeldWithdrawals {
                device: device.to_string(),
                paths,
            })
            .collect())
    }

    /// Withdraws what `device` changed in `folder` after it was lost (`withdraw`), moving it
    /// to the recycle bin, or keeps it as if it had come before.
    pub fn decide_withdrawals(&self, folder: String, device: String, withdraw: bool) -> Result<()> {
        let folder = parse("folder ID", &folder)?;
        let device = parse("device ID", &device)?;
        Ok(self
            .engine()?
            .decide_withdrawals(folder, device, withdraw)?)
    }

    /// The files a scan found deleted and held, because there were so many (ADR 0037).
    pub fn held_deletes(&self, folder: String) -> Result<Vec<String>> {
        Ok(self.engine()?.held_deletes(parse("folder ID", &folder)?)?)
    }

    /// Deletes the held files on every device, each keeping them in its recycle bin.
    pub async fn confirm_deletes(&self, folder: String) -> Result<()> {
        let engine = self.engine()?;
        let folder = parse("folder ID", &folder)?;
        on_runtime(async move { engine.confirm_deletes(folder).await }).await??;
        Ok(())
    }

    /// Brings the held files back from other devices; nothing is deleted.
    pub async fn restore_deleted(&self, folder: String) -> Result<()> {
        let engine = self.engine()?;
        let folder = parse("folder ID", &folder)?;
        on_runtime(async move { engine.restore_deleted(folder).await }).await??;
        Ok(())
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
                confirmed_copies: f.confirmed_copies as u32,
                only_here: f.only_here,
                pinned: f.pinned,
                author: f.author.to_string(),
                author_name: f.author_name,
            })
            .collect())
    }

    /// The full replicas that confirmed holding a file made on this device. Empty for files
    /// made elsewhere.
    pub fn copies(&self, folder: String, path: String) -> Result<Vec<Copy>> {
        Ok(self
            .engine()?
            .copies(parse("folder ID", &folder)?, &path)?
            .into_iter()
            .map(|c| Copy {
                device: c.device.to_string(),
                name: c.name,
                confirmed_ms: c.confirmed_ms,
            })
            .collect())
    }

    /// The upload and download limits in force.
    pub fn limits(&self) -> Result<Limits> {
        let l = self.engine()?.limits();
        Ok(Limits {
            upload: l.upload,
            download: l.download,
            limit_local: l.limit_local,
        })
    }

    /// Changes the upload and download limits until the node stops.
    pub fn set_limits(&self, limits: Limits) -> Result<()> {
        self.engine()?.set_limits(tidefell_engine::Limits {
            upload: limits.upload,
            download: limits.download,
            limit_local: limits.limit_local,
        });
        Ok(())
    }

    /// The versions of a file: the current one, then those kept on this device, newest first.
    pub fn versions(&self, folder: String, path: String) -> Result<Vec<Version>> {
        Ok(self
            .engine()?
            .versions(parse("folder ID", &folder)?, &path)?
            .into_iter()
            .map(|v| Version {
                id: v.id,
                kind: match v.kind {
                    tidefell_engine::VersionKind::Current => VersionKind::Current,
                    tidefell_engine::VersionKind::Previous => VersionKind::Previous,
                    tidefell_engine::VersionKind::Conflict => VersionKind::Conflict,
                },
                size: v.size,
                modified_ms: v.modified_ms,
                author: v.author.map(|d| d.to_string()),
                author_name: v.author_name,
                kept_ms: v.kept_ms,
                expires_ms: v.expires_ms,
            })
            .collect())
    }

    /// The file holding a previous version, to open read-only.
    pub fn version_file(&self, folder: String, path: String, version: String) -> Result<String> {
        Ok(self
            .engine()?
            .version_file(parse("folder ID", &folder)?, &path, &version)?
            .display()
            .to_string())
    }

    /// Writes a read-only copy of a previous or conflicting version, fetching a conflicting one
    /// from peers if needed, and returns its path (ADR 0029).
    pub async fn open_version(
        &self,
        folder: String,
        path: String,
        version: String,
    ) -> Result<String> {
        let engine = self.engine()?;
        let folder = parse("folder ID", &folder)?;
        let file =
            on_runtime(async move { engine.open_version(folder, &path, &version).await }).await??;
        Ok(file.display().to_string())
    }

    /// Decides a conflict on a file (ADR 0029): `keep` is the version to keep, `current` or
    /// `conflict:<id>`, and the others go to every device's recycle bin; or `both`, which keeps
    /// each other version as a file of its own beside it.
    pub async fn resolve(&self, folder: String, path: String, keep: String) -> Result<()> {
        let engine = self.engine()?;
        let folder = parse("folder ID", &folder)?;
        let resolution = if keep == "both" {
            tidefell_engine::Resolution::KeepBoth
        } else {
            tidefell_engine::Resolution::Keep(keep)
        };
        on_runtime(async move { engine.resolve(folder, &path, resolution).await }).await??;
        Ok(())
    }

    /// Makes a previous version the file's content again, on every device.
    pub async fn restore_version(
        &self,
        folder: String,
        path: String,
        version: String,
    ) -> Result<()> {
        let engine = self.engine()?;
        let folder = parse("folder ID", &folder)?;
        on_runtime(async move { engine.restore_version(folder, &path, &version).await }).await??;
        Ok(())
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

    /// Deletes the file at `path` on every device; it goes to their recycle bins. Works on
    /// placeholders too.
    pub async fn delete(&self, folder: String, path: String) -> Result<()> {
        let engine = self.engine()?;
        let folder: FolderId = parse("folder ID", &folder)?;
        on_runtime(async move { engine.delete(folder, &path).await }).await??;
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
            .map(recycled_record)
            .collect())
    }

    /// Permanently deletes one file from this device's recycle bin. Other devices keep their
    /// copies.
    pub fn discard(&self, folder: String, entry: String) -> Result<Recycled> {
        self.engine()?
            .empty_recycle_bin(
                parse("folder ID", &folder)?,
                Some(parse("entry ID", &entry)?),
            )?
            .into_iter()
            .next()
            .map(recycled_record)
            .ok_or(TidefellError::NotFound { detail: entry })
    }

    /// Permanently deletes everything in this device's recycle bin for a folder, and returns
    /// what was removed. Other devices keep their copies.
    pub fn empty_recycle_bin(&self, folder: String) -> Result<Vec<Recycled>> {
        Ok(self
            .engine()?
            .empty_recycle_bin(parse("folder ID", &folder)?, None)?
            .into_iter()
            .map(recycled_record)
            .collect())
    }

    /// Restores a file from the recycle bin: on every device if it was deleted everywhere,
    /// here only if it was removed from this device only.
    pub async fn restore(&self, folder: String, entry: String) -> Result<()> {
        let engine = self.engine()?;
        let folder: FolderId = parse("folder ID", &folder)?;
        let entry = parse("entry ID", &entry)?;
        on_runtime(async move { engine.restore(folder, entry).await }).await??;
        Ok(())
    }

    /// Renames or moves a file, or a directory and everything in it, on every device: for
    /// example one whose name this phone's storage cannot keep (ADR 0033).
    pub async fn rename(&self, folder: String, from: String, to: String) -> Result<()> {
        let engine = self.engine()?;
        let folder: FolderId = parse("folder ID", &folder)?;
        on_runtime(async move { engine.rename(folder, &from, &to).await }).await??;
        Ok(())
    }

    /// Pins a file or directory on this partial replica (ADR 0029): fetched in the background,
    /// kept up to date, never evicted, and not counted against the budget.
    pub async fn pin(&self, folder: String, path: String) -> Result<()> {
        let engine = self.engine()?;
        let folder: FolderId = parse("folder ID", &folder)?;
        on_runtime(async move { engine.pin(folder, &path).await }).await??;
        Ok(())
    }

    /// Removes a pin; what it covered may then be evicted.
    pub async fn unpin(&self, folder: String, path: String) -> Result<()> {
        let engine = self.engine()?;
        let folder: FolderId = parse("folder ID", &folder)?;
        on_runtime(async move { engine.unpin(folder, &path).await }).await??;
        Ok(())
    }

    /// The paths pinned on this device.
    pub fn pins(&self, folder: String) -> Result<Vec<String>> {
        Ok(self.engine()?.pins(parse("folder ID", &folder)?)?)
    }

    /// Removes a file from this device only (ADR 0025). It moves to this device's recycle bin,
    /// stays on every other device, and is not downloaded again until opened or restored.
    pub async fn remove_here(&self, folder: String, path: String) -> Result<()> {
        let engine = self.engine()?;
        let folder: FolderId = parse("folder ID", &folder)?;
        on_runtime(async move { engine.remove_here(folder, &path).await }).await??;
        Ok(())
    }

    /// Every device this one syncs with, connected or not, and how (ADR 0030).
    pub fn peer_states(&self) -> Result<Vec<PeerState>> {
        Ok(self
            .engine()?
            .peer_states()
            .into_iter()
            .map(|p| PeerState {
                device: p.device.to_string(),
                name: p.name,
                connected: p.connected,
                relayed: p.path.map(|path| path == tidefell_engine::PathKind::Relay),
                rtt_ms: p.rtt_ms,
                last_seen_ms: p.last_seen_ms,
                last_error: p.last_error,
                clock_offset_ms: p.clock_offset_ms,
            })
            .collect())
    }

    /// Writes a diagnostics bundle into `dir` and returns where, and what it holds, for the app
    /// to show before the user shares it (ADR 0030). `app` is JSON about the app, such as the
    /// Android API level, the device model and the app's settings. Paths and names are hashed
    /// unless `include_paths`. It reads the disks, so call it off the main thread.
    pub fn diagnostics(
        &self,
        dir: String,
        include_paths: bool,
        app: Option<String>,
    ) -> Result<DiagnosticsBundle> {
        let options = tidefell_engine::BundleOptions {
            include_paths,
            app: app.and_then(|a| serde_json::from_str(&a).ok()),
        };
        let entries = self
            .engine()?
            .diagnostics(&options)
            .map_err(|e| invalid(e.to_string()))?;
        let zip = tidefell_engine::zip_bundle(&entries).map_err(|e| invalid(e.to_string()))?;
        let dir = PathBuf::from(dir);
        std::fs::create_dir_all(&dir).map_err(|e| invalid(e.to_string()))?;
        let path = dir.join("tidefell-diagnostics.zip");
        std::fs::write(&path, zip).map_err(|e| invalid(e.to_string()))?;
        Ok(DiagnosticsBundle {
            path: path.display().to_string(),
            entries: entries
                .into_iter()
                .map(|e| DiagnosticsEntry {
                    name: e.name,
                    text: e.text,
                })
                .collect(),
        })
    }

    /// Checks the node's clock against its peers', its free space, its folders and its
    /// connectivity (ADR 0030). The app adds its own checks, such as All files access. It reads
    /// the folders' disks, so call it off the main thread.
    pub fn doctor(&self) -> Result<Vec<Check>> {
        Ok(self
            .engine()?
            .doctor()
            .into_iter()
            .map(|c| Check {
                name: c.name,
                level: match c.level {
                    tidefell_engine::CheckLevel::Pass => CheckLevel::Pass,
                    tidefell_engine::CheckLevel::Warning => CheckLevel::Warning,
                    tidefell_engine::CheckLevel::Failure => CheckLevel::Failure,
                },
                detail: c.detail,
                advice: c.advice,
            })
            .collect())
    }

    /// File content sent and received since the node started (ADR 0030).
    pub fn transfer(&self) -> Result<Transfer> {
        let t = self.engine()?.transfer();
        Ok(Transfer {
            sent: t.sent,
            received: t.received,
            send_rate: t.send_rate,
            receive_rate: t.receive_rate,
        })
    }

    /// What this device applied to a folder, newest first, at most `limit` changes: who changed
    /// which file, and whether the change came from a peer (ADR 0030).
    pub fn activity(&self, folder: String, limit: u32) -> Result<Vec<Activity>> {
        use tidefell_engine::ChangeKind as Kind;
        Ok(self
            .engine()?
            .activity(parse("folder ID", &folder)?, limit as usize)?
            .into_iter()
            .map(|a| Activity {
                time_ms: a.time_ms,
                kind: match a.kind {
                    Kind::Added => ChangeKind::Added,
                    Kind::Changed => ChangeKind::Changed,
                    Kind::Deleted => ChangeKind::Deleted,
                    Kind::Renamed => ChangeKind::Renamed,
                    Kind::Restored => ChangeKind::Restored,
                    Kind::Conflict => ChangeKind::Conflict,
                },
                path: a.path,
                old_path: a.old_path,
                device: a.device.to_string(),
                device_name: a.device_name,
                local: a.local,
            })
            .collect())
    }

    /// The folder's paths that failed to scan, fetch, write or delete (ADR 0030).
    pub fn path_problems(&self, folder: String) -> Result<Vec<PathProblem>> {
        use tidefell_engine::{Operation as Op, ProblemKind as Kind};
        Ok(self
            .engine()?
            .path_problems(parse("folder ID", &folder)?)?
            .into_iter()
            .map(|p| PathProblem {
                path: p.path,
                operation: match p.operation {
                    Op::Scan => Operation::Scan,
                    Op::Fetch => Operation::Fetch,
                    Op::Write => Operation::Write,
                    Op::Delete => Operation::Delete,
                },
                kind: match p.kind {
                    Kind::PermissionDenied => ProblemKind::PermissionDenied,
                    Kind::DiskFull => ProblemKind::DiskFull,
                    Kind::ContentUnavailable => ProblemKind::ContentUnavailable,
                    Kind::NameNotAllowed => ProblemKind::NameNotAllowed,
                    Kind::ChangedWhileWriting => ProblemKind::ChangedWhileWriting,
                    Kind::Other => ProblemKind::Other,
                },
                message: p.message,
                since_ms: p.since_ms,
                attempts: p.attempts,
            })
            .collect())
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

    /// Keeps a peer connected, with optional direct addresses (`host:port`) and relay URL.
    pub fn add_peer(
        &self,
        device: String,
        addrs: Vec<String>,
        relay: Option<String>,
    ) -> Result<()> {
        Ok(self.engine()?.add_peer(PeerAddr {
            device: parse("device ID", &device)?,
            addrs: addrs
                .iter()
                .map(|a| parse("address", a))
                .collect::<Result<_>>()?,
            relay: relay
                .as_deref()
                .map(|r| parse("relay URL", r))
                .transpose()?,
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
                    E::ConflictDetected { folder, path } => NodeEvent::ConflictDetected {
                        folder: folder.to_string(),
                        path,
                    },
                    E::Crashed(message) => NodeEvent::Crashed { message },
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
    #[expect(
        clippy::too_many_arguments,
        reason = "the settings join_folder takes, one by one"
    )]
    fn add(
        &self,
        id: Option<FolderId>,
        path: String,
        partial_budget: Option<u64>,
        members: Vec<String>,
        retention_days: u32,
        sync: SyncOptions,
        encryption: Option<Encryption>,
    ) -> Result<Folder> {
        let engine = self.engine()?;
        // Adding a folder starts its scanner; foreign threads have no runtime of their own.
        let _runtime = runtime().enter();
        let members = members
            .iter()
            .map(|m| parse("user ID", m))
            .collect::<Result<_>>()?;
        let folder = engine.create_folder(NewFolder {
            id,
            path: PathBuf::from(path),
            mode: ReplicaMode::from_budget(partial_budget),
            members,
            retention: Retention::from_days(retention_days),
            scan_interval: Duration::from_secs(30),
            sync: sync.into(),
            encryption: encryption
                .map(|e| {
                    Ok::<_, TidefellError>(tidefell_engine::Encryption {
                        storage_peers: e
                            .storage_peers
                            .iter()
                            .map(|d| parse("storage peer device ID", d))
                            .collect::<Result<_>>()?,
                        stored: None,
                    })
                })
                .transpose()?,
        })?;
        Ok(folder_record(folder))
    }
}
