//! Free-space floors (ADR 0037): below a drive's floor, downloads wait, listed as problems, and
//! resume by themselves once there is room.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, ProblemKind, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([39; 32]);

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

    async fn start(&self, user: &UserKey, min_free_space: Option<u64>) -> Engine {
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
                confirm_deletes: Default::default(),
                encryption: None,
            }],
            min_free_space,
        };
        let certificate = user.certify(self.key.id(), "test", 0);
        Engine::start(config, self.key.clone(), certificate)
            .await
            .unwrap()
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..1500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_drive_pauses_downloads_until_there_is_room() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    fs::create_dir_all(&a.root).unwrap();
    for n in 0..3 {
        fs::write(a.root.join(format!("file-{n}.txt")), format!("file {n}")).unwrap();
    }
    let engine_a = a.start(&user, None).await;
    engine_a.scan(FOLDER).await.unwrap();
    // More must stay free than any drive holds: as if b's drive were full.
    let engine_b = b.start(&user, Some(u64::MAX / 4)).await;
    engine_b.connect(engine_a.addr()).await.unwrap();

    eventually("b knows of the files and waits for space", || {
        engine_b
            .path_problems(FOLDER)
            .unwrap()
            .iter()
            .filter(|p| p.kind == ProblemKind::DiskFull)
            .count()
            == 3
    })
    .await;
    assert!(!b.root.join("file-0.txt").exists());
    let low = engine_b.folders()[0]
        .low_space
        .clone()
        .expect("the folder says why");
    assert!(low.contains("low on space"), "{low}");

    // Room again: the files arrive, and the problems and the notice go.
    engine_b.set_min_free_space(Some(0));
    eventually("the files arrive", || {
        (0..3).all(|n| b.root.join(format!("file-{n}.txt")).exists())
    })
    .await;
    eventually("the problems settle", || {
        engine_b.path_problems(FOLDER).unwrap().is_empty()
    })
    .await;
    assert_eq!(engine_b.folders()[0].low_space, None);

    engine_a.shutdown().await;
    engine_b.shutdown().await;
}
