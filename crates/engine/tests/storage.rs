//! Content is kept once per device (ADR 0027): files on disk are not copied into the block
//! store, and deleted files are kept for the recycle bin by moving them aside.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode, Retention};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([27; 32]);

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

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.root.join(path)).ok()
    }

    /// Bytes of chunk data in the block store.
    fn stored_bytes(&self) -> u64 {
        fn walk(dir: &Path) -> u64 {
            fs::read_dir(dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| {
                            let path = e.path();
                            if path.is_dir() {
                                walk(&path)
                            } else {
                                e.metadata().map(|m| m.len()).unwrap_or(0)
                            }
                        })
                        .sum()
                })
                .unwrap_or(0)
        }
        walk(&self.data.join("blocks").join("chunks"))
    }

    fn kept_files(&self) -> usize {
        fs::read_dir(self.root.join(".tidefell").join("deleted"))
            .map(|d| d.count())
            .unwrap_or(0)
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

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn content_is_kept_once_and_deletes_stay_restorable() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([9; 32]);
    let a = Replica::new(dir.path(), "a", 50);
    let b = Replica::new(dir.path(), "b", 51);
    let big = noise(3 * 1024 * 1024, 11);
    fs::create_dir_all(&a.root).unwrap();
    fs::write(a.root.join("big.bin"), &big).unwrap();

    let engine_a = a.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(
        a.stored_bytes(),
        0,
        "scanning copies nothing into the block store"
    );
    let engine_b = b.start(&user).await;
    engine_b.connect(engine_a.addr()).await.unwrap();

    // b fetches the file from a's disk, and keeps no second copy once it is written.
    eventually("the file reaches b", || {
        b.read("big.bin").as_deref() == Some(big.as_slice())
    })
    .await;
    eventually("b releases the downloaded chunks", || b.stored_bytes() == 0).await;
    assert_eq!(a.stored_bytes(), 0);

    // Deleted on a by hand: b moves its file aside for the recycle bin instead of copying it.
    fs::remove_file(a.root.join("big.bin")).unwrap();
    engine_a.scan(FOLDER).await.unwrap();
    eventually("the delete reaches b", || b.read("big.bin").is_none()).await;
    eventually("b recycled it", || {
        engine_b.recycle_bin(FOLDER).unwrap().len() == 1
    })
    .await;
    assert_eq!(b.kept_files(), 1);
    assert_eq!(
        b.stored_bytes(),
        0,
        "kept by moving the file, not by copying it"
    );

    // With a gone, b restores it from what it kept, and the kept file goes once it is back.
    engine_a.shutdown().await;
    let entry = engine_b.recycle_bin(FOLDER).unwrap()[0].entry;
    engine_b.restore(FOLDER, entry).await.unwrap();
    eventually("restored on b without a", || {
        b.read("big.bin").as_deref() == Some(big.as_slice())
    })
    .await;
    eventually("the kept file is removed", || b.kept_files() == 0).await;
    assert_eq!(b.stored_bytes(), 0);

    // a, which deleted it by hand and kept nothing, gets it back from b.
    let engine_a = a.start(&user).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("restored on a from b", || {
        a.read("big.bin").as_deref() == Some(big.as_slice())
    })
    .await;

    engine_a.shutdown().await;
    engine_b.shutdown().await;
}
