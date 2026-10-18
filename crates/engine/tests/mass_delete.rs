//! Mass deletes wait for confirmation (ADR 0037): one scan that would delete much of a folder
//! everywhere records nothing until the user confirms, or brings the files back.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    ConfirmDeletes, Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([38; 32]);
const FILES: usize = 150;

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

    async fn start(&self, user: &UserKey, confirm_deletes: ConfirmDeletes) -> Engine {
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
                // Scans are triggered explicitly.
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
                confirm_deletes,
                encryption: None,
            }],
            min_free_space: None,
        };
        let certificate = user.certify(self.key.id(), "test", 0);
        Engine::start(config, self.key.clone(), certificate)
            .await
            .unwrap()
    }

    fn count(&self) -> usize {
        fs::read_dir(&self.root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_file())
            .count()
    }

    fn remove(&self, files: std::ops::Range<usize>) {
        for n in files {
            fs::remove_file(self.root.join(format!("file-{n:03}.txt"))).unwrap();
        }
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

fn held(engine: &Engine) -> usize {
    engine.folders()[0].held_deletes
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_most_of_a_folder_waits_for_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    fs::create_dir_all(&a.root).unwrap();
    for n in 0..FILES {
        fs::write(a.root.join(format!("file-{n:03}.txt")), format!("file {n}")).unwrap();
    }
    let engine_a = a.start(&user, ConfirmDeletes::default()).await;
    engine_a.scan(FOLDER).await.unwrap();
    let engine_b = b.start(&user, ConfirmDeletes::default()).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("the files reach b", || b.count() == FILES).await;

    // Most of the folder gone at once: nothing is deleted anywhere, and the folder says so.
    a.remove(0..120);
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(held(&engine_a), 120);
    assert_eq!(engine_a.held_deletes(FOLDER).unwrap().len(), 120);
    assert_eq!(engine_a.files(FOLDER).unwrap().len(), FILES);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(b.count(), FILES, "no delete reached b");
    // Scanning again changes nothing.
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(held(&engine_a), 120);

    // Restored: the files come back from b, and nothing is held.
    engine_a.restore_deleted(FOLDER).await.unwrap();
    assert_eq!(held(&engine_a), 0);
    eventually("the files come back to a", || a.count() == FILES).await;
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(held(&engine_a), 0);

    // Deleted again and confirmed: the deletes reach b, which keeps them in its recycle bin.
    a.remove(0..120);
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(held(&engine_a), 120);
    engine_a.confirm_deletes(FOLDER).await.unwrap();
    assert_eq!(held(&engine_a), 0);
    eventually("the deletes reach b", || b.count() == FILES - 120).await;
    eventually("b keeps them in its recycle bin", || {
        engine_b.recycle_bin(FOLDER).unwrap().len() == 120
    })
    .await;

    // Fewer than 100 at once is an ordinary delete.
    a.remove(120..125);
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(held(&engine_a), 0);
    eventually("the small delete reaches b", || b.count() == FILES - 125).await;

    engine_a.shutdown().await;
    engine_b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_can_record_every_delete() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 12);
    fs::create_dir_all(&a.root).unwrap();
    for n in 0..FILES {
        fs::write(a.root.join(format!("file-{n:03}.txt")), format!("file {n}")).unwrap();
    }
    let engine = a.start(&user, ConfirmDeletes::Never).await;
    engine.scan(FOLDER).await.unwrap();
    a.remove(0..FILES);
    engine.scan(FOLDER).await.unwrap();
    assert_eq!(held(&engine), 0);
    assert!(engine.files(FOLDER).unwrap().is_empty());
    engine.shutdown().await;
}

#[test]
fn the_threshold_needs_both_a_share_and_a_hundred_files() {
    let over_half = ConfirmDeletes::default();
    assert!(!over_half.holds(99, 100), "fewer than 100");
    assert!(!over_half.holds(100, 200), "exactly half");
    assert!(over_half.holds(101, 200));
    assert!(over_half.holds(100, 100));
    assert!(!ConfirmDeletes::Over(90).holds(150, 200));
    assert!(!ConfirmDeletes::Never.holds(1000, 1000));
}
