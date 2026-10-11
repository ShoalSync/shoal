//! Pause (ADR 0029): a paused folder still records local changes but sends and receives nothing,
//! across restarts, until it resumes.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, Limits, RelayConfig, ReplicaMode, Retention,
    MIN_BYTES_PER_SEC,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([33; 32]);

struct Replica {
    root: PathBuf,
    data: PathBuf,
    key: DeviceKey,
}

impl Replica {
    fn new(dir: &Path, name: &str, seed: u8) -> Self {
        Self {
            root: dir.join(name).join("folder"),
            data: dir.join(name).join("data"),
            key: DeviceKey::from_seed([seed; 32]),
        }
    }

    async fn start(&self, user: &UserKey) -> Engine {
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
                id: FOLDER,
                path: self.root.clone(),
                mode: ReplicaMode::Full,
                members: vec![user.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
            }],
        };
        let certificate = user.certify(self.key.id(), "test", 0);
        Engine::start(config, self.key.clone(), certificate)
            .await
            .unwrap()
    }

    fn write(&self, path: &str, data: &[u8]) {
        let abs = self.root.join(path);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, data).unwrap();
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.root.join(path)).ok()
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn recorded(engine: &Engine, path: &str) -> bool {
    engine.files(FOLDER).unwrap().iter().any(|f| f.path == path)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paused_folder_transfers_nothing_until_it_resumes() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([8; 32]);
    let a = Replica::new(dir.path(), "a", 80);
    let b = Replica::new(dir.path(), "b", 81);
    a.write("first.txt", b"before pausing");
    let engine_a = a.start(&user).await;
    let engine_b = b.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("the first file reaches b", || b.read("first.txt").is_some()).await;

    engine_b.pause(Some(FOLDER)).unwrap();
    assert!(engine_b.folders()[0].paused);
    assert!(!engine_b.paused());
    a.write("from-a.txt", b"made while b is paused");
    engine_a.scan(FOLDER).await.unwrap();
    b.write("from-b.txt", b"made on the paused device");
    engine_b.scan(FOLDER).await.unwrap();
    // b still records its own changes, but nothing moves either way.
    assert!(recorded(&engine_b, "from-b.txt"));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(b.read("from-a.txt").is_none());
    assert!(!recorded(&engine_a, "from-b.txt"));
    // Opening a file that is not here fails instead of fetching it.
    assert!(engine_b.open(FOLDER, "from-a.txt").await.is_err());

    // Still paused after a restart.
    engine_b.shutdown().await;
    let engine_b = b.start(&user).await;
    assert!(engine_b.folders()[0].paused);
    engine_b.connect(engine_a.addr()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(b.read("from-a.txt").is_none());

    engine_b.resume(Some(FOLDER)).unwrap();
    eventually("a's file reaches b after resuming", || {
        b.read("from-a.txt").as_deref() == Some(b"made while b is paused".as_slice())
    })
    .await;
    eventually("b's file reaches a after resuming", || {
        a.read("from-b.txt").as_deref() == Some(b"made on the paused device".as_slice())
    })
    .await;

    // Pausing the whole device stops every folder the same way.
    engine_a.pause(None).unwrap();
    assert!(engine_a.paused());
    b.write("later.txt", b"while a is paused");
    engine_b.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(a.read("later.txt").is_none());
    engine_a.resume(None).unwrap();
    eventually("b's later file reaches a", || a.read("later.txt").is_some()).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pausing_stops_a_download_already_running() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([9; 32]);
    let a = Replica::new(dir.path(), "a", 90);
    let b = Replica::new(dir.path(), "b", 91);
    // About 2 MB of varied data in several chunks: half a minute at the lowest download limit,
    // since each chunk after the first waits for the one before.
    let big: Vec<u8> = (0..2_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    a.write("big.bin", &big);
    let engine_a = a.start(&user).await;
    let engine_b = b.start(&user).await;
    engine_b.set_limits(Limits {
        upload: None,
        download: Some(MIN_BYTES_PER_SEC),
        limit_local: true,
    });
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("b knows of the file", || {
        engine_b
            .files(FOLDER)
            .unwrap()
            .iter()
            .any(|f| f.path == "big.bin")
    })
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    engine_b.pause(Some(FOLDER)).unwrap();
    tokio::time::sleep(Duration::from_secs(35)).await;
    assert!(
        b.read("big.bin").is_none(),
        "the download went on while paused"
    );

    // Resumed without a limit, it completes.
    engine_b.set_limits(Limits::default());
    engine_b.resume(Some(FOLDER)).unwrap();
    eventually("the file arrives after resuming", || {
        b.read("big.bin").as_deref() == Some(big.as_slice())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_joined_again_starts_unpaused() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([10; 32]);
    let a = Replica::new(dir.path(), "a", 92);
    let engine = a.start(&user).await;
    let other = FolderId([34; 32]);
    let config = FolderConfig {
        id: other,
        path: dir.path().join("other"),
        mode: ReplicaMode::Full,
        members: vec![user.id()],
        retention: Retention::Days(30),
        scan_interval: Duration::from_secs(3600),
        sync: Default::default(),
    };
    engine.add_folder(config.clone()).unwrap();
    engine.pause(Some(other)).unwrap();
    engine.remove_folder(other).unwrap();
    engine.add_folder(config).unwrap();
    let joined = engine
        .folders()
        .into_iter()
        .find(|f| f.id == other)
        .unwrap();
    assert!(!joined.paused);
}
