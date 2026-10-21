//! What the provider's tests share: a NAS and a laptop engine for one folder, the laptop's a
//! sync root.
#![allow(dead_code)]

use std::fs;
use std::net::SocketAddr;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tidefell_engine::{Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode, Retention};
use tidefell_fs_cfapi::{connect, EngineFolder, SyncRoot};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

pub const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

pub struct Device {
    pub data: PathBuf,
    pub root: PathBuf,
    pub key: DeviceKey,
    pub user: UserKey,
    pub mode: ReplicaMode,
    pub folder: FolderId,
}

impl Device {
    pub async fn start(&self) -> Arc<Engine> {
        fs::create_dir_all(&self.root).unwrap();
        let config = EngineConfig {
            data_dir: self.data.clone(),
            metadata_dir: None,
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            limits: Default::default(),
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            peers: Vec::new(),
            folders: vec![FolderConfig {
                id: self.folder,
                path: self.root.clone(),
                mode: self.mode,
                members: vec![self.user.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
                confirm_deletes: Default::default(),
                encryption: None,
            }],
            min_free_space: None,
        };
        let certificate = self.user.certify(self.key.id(), "test", 0);
        Arc::new(
            Engine::start(config, self.key.clone(), certificate)
                .await
                .unwrap(),
        )
    }

    pub fn write(&self, path: &str, data: &[u8]) {
        let abs = self.abs(path);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, data).unwrap();
    }

    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.abs(path)).ok()
    }

    pub fn abs(&self, path: &str) -> PathBuf {
        path.split('/')
            .fold(self.root.clone(), |p, part| p.join(part))
    }
}

/// A NAS (full) and a laptop (partial, its folder in the user's profile), one folder.
pub struct Pair {
    pub nas: Device,
    pub laptop: Device,
    pub _dirs: (tempfile::TempDir, tempfile::TempDir),
}

/// The engines' and the provider's logs, shown with a test's output when it fails.
pub fn logs() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            "provider=info,tidefell_engine=debug,tidefell_net=debug,tidefell_fs_cfapi=debug",
        )
        .with_test_writer()
        .try_init();
}

pub fn pair(seed: u8) -> Pair {
    logs();
    let data = tempfile::tempdir().unwrap();
    let profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
    let roots = tempfile::Builder::new()
        .prefix("Tidefell provider ")
        .tempdir_in(profile)
        .unwrap();
    let user = UserKey::from_seed([seed; 32]);
    let folder = FolderId([seed; 32]);
    let nas = Device {
        data: data.path().join("nas"),
        root: data.path().join("nas-folder"),
        key: DeviceKey::from_seed([seed + 1; 32]),
        user: user.clone(),
        mode: ReplicaMode::Full,
        folder,
    };
    let laptop = Device {
        data: data.path().join("laptop"),
        root: roots.path().join("Tidefell"),
        key: DeviceKey::from_seed([seed + 2; 32]),
        user,
        mode: ReplicaMode::Partial {
            budget_bytes: 1 << 20,
        },
        folder,
    };
    Pair {
        nas,
        laptop,
        _dirs: (data, roots),
    }
}

pub async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Runs a call that blocks on the engine, as the provider's do, off the runtime's threads.
pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    tokio::task::spawn_blocking(f).await.unwrap()
}

pub async fn connect_root(engine: &Arc<Engine>, device: &Device) -> SyncRoot {
    let (engine, root, folder) = (engine.clone(), device.root.clone(), device.folder);
    let runtime = tokio::runtime::Handle::current();
    blocking(move || {
        connect(
            EngineFolder::new(engine, folder, runtime),
            &root,
            folder,
            "Tidefell test",
        )
        .unwrap()
    })
    .await
}

pub async fn changed(root: &Arc<SyncRoot>) {
    let root = root.clone();
    blocking(move || root.changed().unwrap()).await;
}

pub fn is_placeholder_not_on_disk(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_attributes() & RECALL_ON_DATA_ACCESS != 0)
}

pub fn on_disk(engine: &Engine, folder: FolderId, path: &str) -> bool {
    engine
        .files(folder)
        .unwrap()
        .iter()
        .any(|f| f.path == path && f.on_disk)
}

pub fn entry_at(engine: &Engine, folder: FolderId, path: &str) -> Option<tidefell_proto::EntryId> {
    engine
        .files(folder)
        .unwrap()
        .into_iter()
        .find(|f| f.path == path)
        .map(|f| f.entry)
}

pub fn cmd(args: &[&std::ffi::OsStr]) {
    let status = std::process::Command::new("cmd")
        .arg("/C")
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "{args:?}");
}
