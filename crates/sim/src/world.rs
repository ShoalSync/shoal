//! Runs steps on several engines and checks what must hold once they are done (ADR 0036).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, PeerAddr, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_net::Network;
use tidefell_proto::{DeviceId, FolderId};

use crate::ops::{path, Op, Rng};

const FOLDER: FolderId = FolderId([7; 32]);
/// The folder's reserved directory, where the engine keeps what it replaces (ADR 0027).
const RESERVED: &str = ".tidefell";
/// How long the engines get to agree once the network heals, unless `TIDEFELL_SIM_SETTLE_MS`
/// says otherwise.
fn settle_time() -> Duration {
    std::env::var("TIDEFELL_SIM_SETTLE_MS")
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(Duration::from_secs(60), Duration::from_millis)
}

/// Why a simulation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The step it failed at, or `None` if it failed once all steps were done.
    pub step: Option<usize>,
    pub message: String,
    /// Each device's files after each step, of the run that failed: what a failure that does
    /// not happen again, as one only under load, leaves to go on.
    pub history: Vec<String>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.step {
            Some(step) => write!(f, "at step {step}: {}", self.message),
            None => write!(f, "once settled: {}", self.message),
        }
    }
}

fn fail(step: Option<usize>, message: impl Into<String>) -> Failure {
    Failure {
        step,
        history: Vec::new(),
        message: message.into(),
    }
}

type Hash = [u8; 32];

fn hash(bytes: &[u8]) -> Hash {
    *blake3::hash(bytes).as_bytes()
}

struct Device {
    name: String,
    root: PathBuf,
    data: PathBuf,
    key: DeviceKey,
    engine: Option<Engine>,
    /// Content seen in the folder on this device, with where and at which step it was first.
    seen: HashMap<Hash, (String, usize)>,
    /// Content the user removed from this device: deleted, or written or moved over.
    removed: HashSet<Hash>,
    /// How far its clock is from the true time, in milliseconds.
    clock_ms: i64,
    /// Whether its disk is full.
    full: bool,
}

struct World {
    network: Network,
    user: UserKey,
    devices: Vec<Device>,
    /// The device revoked, if one was (ADR 0040): left out of the comparisons.
    revoked: Option<usize>,
    /// The folder is an encrypted one (ADR 0023).
    encrypted: bool,
    /// For an encrypted folder, a storage peer of another user's that stores it for the
    /// devices' user (ADR 0023): left out of the comparisons, and never holds a key.
    storage: Option<Engine>,
    storage_key: DeviceKey,
    /// Each device's files after each step, for a failure to show.
    history: Vec<String>,
}

/// Runs `ops` on `devices` devices in a directory under `base`, then heals the network, lets
/// the engines settle and checks that:
///
/// - every device not revoked has the same files, and the same index;
/// - no content is lost: what a device had and the user did not remove is still on it, live or
///   kept (ADR 0027, ADR 0029);
/// - no engine failed;
/// - in an encrypted folder, the storage peer never got a key.
pub async fn run(ops: &[Op], devices: u8, base: &Path) -> Result<(), Failure> {
    let dir = tempfile::tempdir_in(base).map_err(|e| fail(None, e.to_string()))?;
    let mut world = World::new(dir.path(), devices);
    world.encrypted = ops.first() == Some(&Op::Encrypted);
    let result = match world.encrypted {
        true => world
            .start_storage(dir.path())
            .await
            .map_err(|e| fail(None, e)),
        false => Ok(()),
    };
    let result = match result {
        Ok(()) => world.run(ops).await,
        Err(e) => Err(e),
    };
    world.stop().await;
    let result = result.map_err(|f| Failure {
        history: std::mem::take(&mut world.history),
        ..f
    });
    // Traces name each run by its directory, so a failure says which run's trace is its own.
    if std::env::var_os("TIDEFELL_SIM_TRACE").is_some() {
        let run = dir
            .path()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        return result.map_err(|f| Failure {
            message: format!("[{}] {}", run.unwrap_or_default(), f.message),
            ..f
        });
    }
    result
}

impl World {
    fn new(dir: &Path, devices: u8) -> Self {
        let devices = (0..devices)
            .map(|n| {
                let name = format!("d{n}");
                Device {
                    root: dir.join(&name).join("folder"),
                    data: dir.join(&name).join("data"),
                    key: DeviceKey::from_seed([n.wrapping_add(10); 32]),
                    name,
                    engine: None,
                    seen: HashMap::new(),
                    removed: HashSet::new(),
                    clock_ms: 0,
                    full: false,
                }
            })
            .collect();
        Self {
            network: Network::new(),
            user: UserKey::from_seed([1; 32]),
            devices,
            revoked: None,
            encrypted: false,
            storage: None,
            storage_key: DeviceKey::from_seed([30; 32]),
            history: Vec::new(),
        }
    }

    /// Starts the storage peer: its operator's user is not a member, and it stores the folder
    /// for the devices' user.
    async fn start_storage(&mut self, dir: &Path) -> Result<(), String> {
        let data = dir.join("storage").join("data");
        let config = EngineConfig {
            data_dir: data.clone(),
            metadata_dir: None,
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            limits: Default::default(),
            bind: None,
            peers: self
                .devices
                .iter()
                .map(|d| PeerAddr {
                    device: d.key.id(),
                    addrs: Vec::new(),
                    relay: None,
                })
                .collect(),
            folders: vec![FolderConfig {
                id: FOLDER,
                path: data.join("stored"),
                mode: ReplicaMode::Full,
                members: vec![self.user.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
                confirm_deletes: Default::default(),
                encryption: Some(tidefell_engine::Encryption {
                    storage_peers: Vec::new(),
                    stored: Some(tidefell_engine::Stored {
                        quota_bytes: u64::MAX,
                    }),
                }),
            }],
            min_free_space: Some(0),
        };
        let operator = UserKey::from_seed([3; 32]);
        let certificate = operator.certify(self.storage_key.id(), "storage", 0);
        let engine =
            Engine::start_simulated(config, self.storage_key.clone(), certificate, &self.network)
                .await
                .map_err(|e| format!("the storage peer did not start: {e}"))?;
        engine.set_retry_interval(Duration::from_millis(50));
        self.storage = Some(engine);
        Ok(())
    }

    async fn start(&mut self, n: usize) -> Result<(), String> {
        let mut ids: Vec<DeviceId> = self.devices.iter().map(|d| d.key.id()).collect();
        let storage = self.storage_key.id();
        if self.encrypted {
            ids.push(storage);
        }
        let device = &mut self.devices[n];
        let config = EngineConfig {
            data_dir: device.data.clone(),
            metadata_dir: None,
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            limits: Default::default(),
            bind: None,
            peers: ids
                .iter()
                .filter(|id| **id != device.key.id())
                .map(|id| PeerAddr {
                    device: *id,
                    addrs: Vec::new(),
                    relay: None,
                })
                .collect(),
            // Added at runtime, so its sync options and members can change (ADR 0029).
            folders: Vec::new(),
            min_free_space: Some(0),
        };
        let certificate = self.user.certify(device.key.id(), &device.name, 0);
        let engine =
            Engine::start_simulated(config, device.key.clone(), certificate, &self.network)
                .await
                .map_err(|e| format!("{} did not start: {e}", device.name))?;
        // Kept in the runtime settings from then on.
        let folder = FolderConfig {
            id: FOLDER,
            path: device.root.clone(),
            mode: ReplicaMode::Full,
            members: vec![self.user.id()],
            // Long enough that nothing kept expires during a run.
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(3600),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: self.encrypted.then(|| tidefell_engine::Encryption {
                storage_peers: vec![storage],
                stored: None,
            }),
        };
        if engine.status(FOLDER).is_err() {
            engine
                .add_folder(folder)
                .map_err(|e| format!("{} could not add the folder: {e}", device.name))?;
        }
        engine.set_retry_interval(Duration::from_millis(50));
        engine.set_clock_offset(device.clock_ms);
        engine.set_min_free_space(Some(if device.full { u64::MAX / 4 } else { 0 }));
        device.engine = Some(engine);
        Ok(())
    }

    async fn stop(&mut self) {
        for device in &mut self.devices {
            if let Some(engine) = device.engine.take() {
                engine.shutdown().await;
            }
        }
        if let Some(engine) = self.storage.take() {
            engine.shutdown().await;
        }
    }

    async fn run(&mut self, ops: &[Op]) -> Result<(), Failure> {
        for n in 0..self.devices.len() {
            fs::create_dir_all(&self.devices[n].root).map_err(|e| fail(None, e.to_string()))?;
            self.start(n).await.map_err(|e| fail(None, e))?;
        }
        for (step, op) in ops.iter().enumerate() {
            self.apply(step, op)
                .await
                .map_err(|e| fail(Some(step), format!("{op:?}: {e}")))?;
            self.observe(step).map_err(|e| fail(Some(step), e))?;
        }
        self.settle().await.map_err(|e| fail(None, e))?;
        self.check_kept().map_err(|e| fail(None, e))?;
        if let Some(storage) = &self.storage {
            match storage.folder_key_generation(FOLDER) {
                Ok(None) => {}
                held => {
                    return Err(fail(
                        None,
                        format!("the storage peer holds a key: {held:?}"),
                    ))
                }
            }
        }
        Ok(())
    }

    fn engine(&self, n: usize) -> Result<&Engine, String> {
        self.devices[n]
            .engine
            .as_ref()
            .ok_or_else(|| format!("{} is not running", self.devices[n].name))
    }

    async fn scan(&self, n: usize) -> Result<(), String> {
        self.engine(n)?
            .scan(FOLDER)
            .await
            .map_err(|e| format!("{} could not scan: {e}", self.devices[n].name))
    }

    async fn apply(&mut self, step: usize, op: &Op) -> Result<(), String> {
        let count = self.devices.len();
        // `TIDEFELL_SIM_SKIP` names kinds of step to leave out, to tell which a failure needs.
        if let Ok(skip) = std::env::var("TIDEFELL_SIM_SKIP") {
            let kind = format!("{op:?}");
            if skip
                .split(',')
                .any(|k| !k.is_empty() && kind.starts_with(k))
            {
                return Ok(());
            }
        }
        match *op {
            Op::Write {
                device,
                path: p,
                size,
                scan,
            } => {
                let n = usize::from(device) % count;
                let rel = path(p);
                let content = content(step, size.bytes());
                // Written new, so a file the engine put there in the meantime is never written
                // over unnoticed.
                if self.devices[n].replace(&rel) {
                    let written = fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(self.devices[n].root.join(&rel))
                        .and_then(|mut f| std::io::Write::write_all(&mut f, &content));
                    if written.is_ok() && scan {
                        self.scan(n).await?;
                    }
                }
            }
            Op::Rename { device, file, to } => {
                let n = usize::from(device) % count;
                let files = files(&self.devices[n].root);
                let Some(from) = pick(&files, file) else {
                    return Ok(());
                };
                let to = path(to);
                if from == to || !self.devices[n].replace(&to) {
                    return Ok(());
                }
                let root = &self.devices[n].root;
                // In one step, as a user's `mv` is, and only if nothing is at `to`: a file the
                // engine put at `to` in the meantime is not replaced unnoticed, and one it put at
                // `from` is not removed with what the user moved, as linking then unlinking
                // would. The file may also have gone since it was listed.
                rename_noreplace(&root.join(&from), &root.join(&to));
            }
            Op::Delete { device, file } => {
                let n = usize::from(device) % count;
                let files = files(&self.devices[n].root);
                if let Some(rel) = pick(&files, file) {
                    self.devices[n].take(&rel);
                }
            }
            Op::DeleteDir { device, dir } => {
                let n = usize::from(device) % count;
                let dirs = dirs(&self.devices[n].root);
                if let Some(rel) = pick(&dirs, dir) {
                    self.devices[n].take(&rel);
                }
            }
            Op::Scan { device } => self.scan(usize::from(device) % count).await?,
            Op::Partition { sides } => {
                let sides = self
                    .devices
                    .iter()
                    .enumerate()
                    .map(|(n, d)| (d.key.id(), u32::from(sides >> n & 1)))
                    // The storage peer takes the next bit, so members can meet only through it.
                    .chain(std::iter::once((
                        self.storage_key.id(),
                        u32::from(sides >> count & 1),
                    )));
                self.network.partition(sides);
            }
            Op::Heal => self.network.heal(),
            Op::Encrypted => {}
            Op::Restart { device } => {
                let n = usize::from(device) % count;
                if let Some(engine) = self.devices[n].engine.take() {
                    engine.shutdown().await;
                }
                self.start(n).await?;
            }
            Op::Wait { ms } => tokio::time::sleep(Duration::from_millis(u64::from(ms))).await,
            Op::Clock { device, minutes } => {
                let n = usize::from(device) % count;
                self.devices[n].clock_ms = i64::from(minutes) * 60_000;
                self.engine(n)?.set_clock_offset(self.devices[n].clock_ms);
            }
            Op::LoseMetadata { device } => {
                let n = usize::from(device) % count;
                // Only once the devices agree: a version a device made and sent to nobody is
                // lost with its metadata, and if its user had overwritten the only copy of the
                // version before it, no device can ever have that one, which stays unavailable
                // as it should (ADR 0034).
                self.settle().await?;
                if let Some(engine) = self.devices[n].engine.take() {
                    engine.shutdown().await;
                }
                let metadata = self.devices[n].data.join("metadata");
                if let Ok(entries) = fs::read_dir(&metadata) {
                    for entry in entries.flatten() {
                        let _ = fs::remove_file(entry.path());
                    }
                }
                self.start(n).await?;
            }
            Op::Disk { device, full } => {
                let n = usize::from(device) % count;
                self.devices[n].full = full;
                self.engine(n)?
                    .set_min_free_space(Some(if full { u64::MAX / 4 } else { 0 }));
            }
            Op::SendOnly { device, on } => {
                let n = usize::from(device) % count;
                self.send_only(n, on)?;
            }
            Op::Revoke {
                by,
                device,
                lost_ago_ms,
            } => {
                if self.revoked.is_none() {
                    let by = usize::from(by) % count;
                    let mut n = usize::from(device) % count;
                    if n == by {
                        n = (n + 1) % count;
                    }
                    let id = self.devices[n].key.id();
                    let engine = self.engine(by)?;
                    let lost = lost_ago_ms.map(|ago| engine.now_ms() - i64::from(ago));
                    engine
                        .revoke_device(&self.user, id, lost)
                        .map_err(|e| format!("{} could not revoke: {e}", self.devices[by].name))?;
                    self.revoked = Some(n);
                }
            }
            Op::Member { device, add } => {
                let n = usize::from(device) % count;
                let other = UserKey::from_seed([2; 32]).id();
                let engine = self.engine(n)?;
                let changed = if add {
                    engine.add_member(FOLDER, other)
                } else {
                    engine.remove_member(FOLDER, other)
                };
                changed.map_err(|e| {
                    format!("{} could not change members: {e}", self.devices[n].name)
                })?;
            }
        }
        Ok(())
    }

    /// Notes the content each device has now, and fails if an engine has. Every device's files
    /// go in the history, and with `TIDEFELL_SIM_TRACE` set are printed too.
    fn observe(&mut self, step: usize) -> Result<(), String> {
        let trace = std::env::var_os("TIDEFELL_SIM_TRACE").is_some();
        for device in &mut self.devices {
            self.history.push(format!(
                "after step {step}, {}: {}",
                device.name,
                listing(&device.root)
            ));
            if trace {
                let run = device
                    .root
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::file_name)
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                eprintln!(
                    "[{run}] after step {step}, {}: {}",
                    device.name,
                    listing(&device.root)
                );
            }
            if let Some(failure) = device.engine.as_ref().and_then(Engine::failure) {
                return Err(format!("{} failed: {failure}", device.name));
            }
            for (path, content) in tree(&device.root) {
                device.seen.entry(content).or_insert((path, step));
            }
        }
        if let Some(failure) = self.storage.as_ref().and_then(Engine::failure) {
            return Err(format!("the storage peer failed: {failure}"));
        }
        Ok(())
    }

    fn send_only(&self, n: usize, on: bool) -> Result<(), String> {
        let sync = tidefell_engine::SyncOptions {
            direction: if on {
                tidefell_engine::Direction::SendOnly
            } else {
                tidefell_engine::Direction::Both
            },
            ..Default::default()
        };
        self.engine(n)?.set_sync_options(FOLDER, sync).map_err(|e| {
            format!(
                "{} could not change its sync options: {e}",
                self.devices[n].name
            )
        })
    }

    /// Heals the network, frees every disk, makes every device sync both ways again, scans
    /// every device and waits for them all to agree.
    async fn settle(&mut self) -> Result<(), String> {
        self.network.heal();
        for n in 0..self.devices.len() {
            self.devices[n].full = false;
            self.engine(n)?.set_min_free_space(Some(0));
            self.send_only(n, false)?;
        }
        for n in 0..self.devices.len() {
            self.scan(n).await?;
        }
        let started = Instant::now();
        let mut agreed = 0;
        let mut last = String::new();
        let limit = settle_time();
        while started.elapsed() < limit {
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.observe(usize::MAX)?;
            match self.disagreement() {
                None => {
                    agreed += 1;
                    if agreed == 3 {
                        return Ok(());
                    }
                }
                Some(what) => {
                    agreed = 0;
                    last = what;
                }
            }
        }
        let mut state = String::new();
        for (n, device) in self.devices.iter().enumerate() {
            let index: Vec<String> = self
                .engine(n)?
                .files(FOLDER)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|f| {
                    let entry = f.entry.to_string();
                    format!(
                        "{} [{} by {}{}]",
                        f.path,
                        entry.get(..8).unwrap_or_default(),
                        f.author_name,
                        if f.on_disk { "" } else { ", not on disk" }
                    )
                })
                .collect();
            state.push_str(&format!(
                "\n  {}: index {index:?}; files {}",
                device.name,
                listing(&device.root)
            ));
        }
        Err(format!("did not settle: {last}{state}"))
    }

    /// How the devices differ, if they do.
    fn disagreement(&self) -> Option<String> {
        let mut first: Option<(&str, BTreeMap<String, Hash>, Vec<String>)> = None;
        for (n, device) in self.devices.iter().enumerate() {
            // A revoked device syncs with no one (ADR 0040).
            if self.revoked == Some(n) {
                continue;
            }
            let engine = self.engine(n).ok()?;
            let status = engine.status(FOLDER).ok()?;
            if status.pending > 0 {
                let waiting: Vec<String> = engine
                    .files(FOLDER)
                    .ok()?
                    .into_iter()
                    .filter(|f| !f.on_disk)
                    .map(|f| f.path)
                    .collect();
                let problems: Vec<String> = engine
                    .path_problems(FOLDER)
                    .ok()?
                    .into_iter()
                    .map(|p| format!("{}: {}", p.path, p.message))
                    .collect();
                return Some(format!(
                    "{} has {} pending: not on disk {waiting:?}, problems {problems:?}",
                    device.name, status.pending
                ));
            }
            let disk = tree(&device.root);
            let mut index: Vec<String> = engine
                .files(FOLDER)
                .ok()?
                .into_iter()
                .map(|f| f.path)
                .collect();
            index.sort();
            match &first {
                None => first = Some((&device.name, disk, index)),
                Some((name, other_disk, other_index)) => {
                    if *other_disk != disk {
                        return Some(format!(
                            "{name} and {} have different files: {}",
                            device.name,
                            difference(other_disk, &disk)
                        ));
                    }
                    if *other_index != index {
                        return Some(format!("{name} and {} have different indexes", device.name));
                    }
                }
            }
        }
        None
    }

    /// Fails if a device lost content it had that its user did not remove.
    fn check_kept(&self) -> Result<(), String> {
        for device in &self.devices {
            let kept: HashSet<Hash> = all_content(&device.root);
            let lost: Vec<String> = device
                .seen
                .iter()
                .filter(|(c, _)| !kept.contains(*c) && !device.removed.contains(*c))
                .map(|(_, (path, step))| format!("{path} as seen after step {step}"))
                .collect();
            if !lost.is_empty() {
                return Err(format!(
                    "{} lost what it had of {lost:?}, which its user did not remove; it has {}",
                    device.name,
                    listing(&device.root)
                ));
            }
        }
        Ok(())
    }
}

impl Device {
    /// Makes room for a file at `rel`, noting what the user replaces there; false if a folder
    /// is in the way, or a file where a folder is needed.
    fn replace(&mut self, rel: &str) -> bool {
        let abs = self.root.join(rel);
        if abs.is_dir() {
            return false;
        }
        if let Some(parent) = abs.parent() {
            if fs::create_dir_all(parent).is_err() {
                return false;
            }
        }
        if abs.is_file() {
            self.take(rel);
        }
        true
    }

    /// Removes what is at `rel`, a file or a folder, noting its content as removed by the user.
    /// It is moved out of the folder in one step first, so what is noted is exactly what was
    /// removed, whatever the engine does meanwhile.
    fn take(&mut self, rel: &str) {
        let Some(device_dir) = self.root.parent() else {
            return;
        };
        let away = device_dir.join(format!("taken-{}", self.removed.len()));
        if fs::rename(self.root.join(rel), &away).is_err() {
            return;
        }
        if away.is_dir() {
            self.removed.extend(all_content(&away));
            let _ = fs::remove_dir_all(&away);
        } else {
            if let Ok(bytes) = fs::read(&away) {
                self.removed.insert(hash(&bytes));
            }
            let _ = fs::remove_file(&away);
        }
    }
}

/// Content no other step writes.
fn content(step: usize, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(step as u64);
    let mut out = format!("step {step}\n").into_bytes();
    while out.len() < len {
        out.extend_from_slice(&rng.next().to_le_bytes());
    }
    out.truncate(len.max(16));
    out
}

/// Renames `from` to `to` in one step, unless something is at `to`.
#[cfg(unix)]
fn rename_noreplace(from: &Path, to: &Path) {
    let _ = rustix::fs::renameat_with(
        rustix::fs::CWD,
        from,
        rustix::fs::CWD,
        to,
        rustix::fs::RenameFlags::NOREPLACE,
    );
}

/// Renames `from` to `to`, unless something is at `to`. The simulation runs on Unix; elsewhere
/// this only builds, and checks then renames.
#[cfg(not(unix))]
fn rename_noreplace(from: &Path, to: &Path) {
    if !to.exists() {
        let _ = fs::rename(from, to);
    }
}

fn pick(items: &[String], n: u8) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    items.get(usize::from(n) % items.len()).cloned()
}

/// Files under `root` outside the reserved directory, by path, in order.
fn files(root: &Path) -> Vec<String> {
    tree(root).into_keys().collect()
}

/// Folders under `root` outside the reserved directory, in order.
fn dirs(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(root, root, &mut |rel, path| {
        if path.is_dir() {
            out.push(rel.to_owned());
        }
    });
    out.sort();
    out
}

/// The folder as the user sees it: each file's path and content.
fn tree(root: &Path) -> BTreeMap<String, Hash> {
    let mut out = BTreeMap::new();
    walk(root, root, &mut |rel, path| {
        if path.is_file() {
            if let Ok(bytes) = fs::read(path) {
                out.insert(rel.to_owned(), hash(&bytes));
            }
        }
    });
    out
}

/// Every content in the folder, the reserved directory included.
fn all_content(root: &Path) -> HashSet<Hash> {
    let mut out = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                out.insert(hash(&bytes));
            }
        }
    }
    out
}

fn walk(root: &Path, dir: &Path, visit: &mut impl FnMut(&str, &Path)) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel == RESERVED || rel.starts_with(&format!("{RESERVED}/")) {
            continue;
        }
        visit(&rel, &path);
        if path.is_dir() {
            walk(root, &path, visit);
        }
    }
}

/// Every file in the folder, the reserved directory included, with the step that wrote it.
fn listing(root: &Path) -> String {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                let first = bytes.split(|b| *b == b'\n').next().unwrap_or_default();
                out.push(format!("{rel} ({})", String::from_utf8_lossy(first)));
            }
        }
    }
    out.sort();
    out.join(", ")
}

/// The paths two trees differ at, for a failure's message.
fn difference(a: &BTreeMap<String, Hash>, b: &BTreeMap<String, Hash>) -> String {
    let keys: BTreeSet<&String> = a.keys().chain(b.keys()).collect();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .take(5)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}
