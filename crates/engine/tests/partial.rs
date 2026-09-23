//! Partial replicas (ADR 0016): placeholders, fetching on open, writes, and eviction to the
//! budget.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, Error, FolderConfig, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([9; 32]);

struct Device {
    dir: PathBuf,
    key: DeviceKey,
    user: UserKey,
    mode: ReplicaMode,
}

impl Device {
    fn new(root: &Path, name: &str, seed: u8, user: &UserKey, mode: ReplicaMode) -> Self {
        Self {
            dir: root.join(name),
            key: DeviceKey::from_seed([seed; 32]),
            user: user.clone(),
            mode,
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.join("folder")
    }

    async fn start(&self) -> Engine {
        let config = EngineConfig {
            data_dir: self.dir.join("data"),
            relay: RelayConfig::Disabled,
            address_lookup: false,
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            peers: Vec::new(),
            folders: vec![FolderConfig {
                id: FOLDER,
                path: self.root(),
                mode: self.mode,
                members: vec![self.user.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
            }],
        };
        let certificate = self.user.certify(self.key.id(), "test", 0);
        Engine::start(config, self.key.clone(), certificate)
            .await
            .unwrap()
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.root().join(path)).ok()
    }

    fn write(&self, path: &str, data: &[u8]) {
        fs::create_dir_all(self.root()).unwrap();
        fs::write(self.root().join(path), data).unwrap();
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

fn on_disk(engine: &Engine, path: &str) -> bool {
    engine
        .files(FOLDER)
        .unwrap()
        .iter()
        .any(|f| f.path == path && f.on_disk)
}

/// Opens and closes `path`, then waits a little so last-use times differ.
async fn touch(engine: &Engine, path: &str) -> Vec<u8> {
    let local = engine.open(FOLDER, path).await.unwrap();
    let data = fs::read(local).unwrap();
    engine.close(FOLDER, path).unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    data
}

#[tokio::test(flavor = "multi_thread")]
async fn placeholders_open_on_demand_and_evict_to_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let nas = Device::new(tmp.path(), "nas", 20, &alice, ReplicaMode::Full);
    let phone = Device::new(
        tmp.path(),
        "phone",
        21,
        &alice,
        ReplicaMode::Partial { budget_bytes: 25 },
    );
    nas.write("a.txt", b"aaaaaaaaaa");
    nas.write("b.txt", b"bbbbbbbbbb");
    nas.write("c.txt", b"cccccccccc");

    let engine_nas = nas.start().await;
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_phone = phone.start().await;
    engine_phone.connect(engine_nas.addr()).await.unwrap();

    // Metadata arrives; content stays on the NAS.
    eventually("phone lists the files", || {
        engine_phone.files(FOLDER).unwrap().len() == 3
    })
    .await;
    engine_phone.materialize(FOLDER).await.unwrap();
    assert!(phone.read("a.txt").is_none());
    assert!(!on_disk(&engine_phone, "a.txt"));
    assert_eq!(engine_phone.status(FOLDER).unwrap().pending, 0);
    assert!(matches!(
        engine_phone.open(FOLDER, "missing.txt").await,
        Err(Error::NoSuchFile(_))
    ));

    // Opening fetches.
    assert_eq!(touch(&engine_phone, "a.txt").await, b"aaaaaaaaaa");
    assert!(on_disk(&engine_phone, "a.txt"));
    assert_eq!(touch(&engine_phone, "b.txt").await, b"bbbbbbbbbb");

    // A third file exceeds the 25-byte budget: the least recently used, a.txt, is evicted.
    let c = engine_phone.open(FOLDER, "c.txt").await.unwrap();
    assert_eq!(fs::read(&c).unwrap(), b"cccccccccc");
    assert!(phone.read("a.txt").is_none());
    assert!(!on_disk(&engine_phone, "a.txt"));
    assert!(on_disk(&engine_phone, "b.txt"));

    // Open files are never evicted: with c.txt pinned, opening a.txt again evicts b.txt.
    assert_eq!(touch(&engine_phone, "a.txt").await, b"aaaaaaaaaa");
    assert!(on_disk(&engine_phone, "c.txt"));
    assert!(!on_disk(&engine_phone, "b.txt"));
    engine_phone.close(FOLDER, "c.txt").unwrap();

    // A remote edit updates a materialised file and leaves placeholders alone.
    nas.write("a.txt", b"AAAAAAAAAA");
    nas.write("b.txt", b"BBBBBBBBBB");
    engine_nas.scan(FOLDER).await.unwrap();
    eventually("phone updates its copy of a.txt", || {
        phone.read("a.txt").as_deref() == Some(b"AAAAAAAAAA".as_slice())
    })
    .await;
    engine_phone.materialize(FOLDER).await.unwrap();
    assert!(phone.read("b.txt").is_none());

    // A new file written through the engine reaches the NAS.
    let source = tmp.path().join("upload.tmp");
    fs::write(&source, b"from the phone").unwrap();
    engine_phone
        .write(FOLDER, "photos/new.txt", &source)
        .await
        .unwrap();
    assert!(!source.exists(), "the source is moved into the folder");
    eventually("the NAS has the phone's file", || {
        nas.read("photos/new.txt").as_deref() == Some(b"from the phone".as_slice())
    })
    .await;

    engine_phone.shutdown().await;
    engine_nas.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn files_made_here_are_kept_until_a_peer_has_them() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([2; 32]);
    let phone = Device::new(
        tmp.path(),
        "phone",
        30,
        &alice,
        ReplicaMode::Partial { budget_bytes: 15 },
    );
    phone.write("one.txt", b"first local file");
    phone.write("two.txt", b"second local file");
    let engine_phone = phone.start().await;
    engine_phone.scan(FOLDER).await.unwrap();

    // Over budget, but no peer holds either file: nothing is evicted.
    touch(&engine_phone, "two.txt").await;
    assert!(phone.read("one.txt").is_some());
    assert!(phone.read("two.txt").is_some());

    // Once a full replica has them, the least recently used one goes.
    let nas = Device::new(tmp.path(), "nas", 31, &alice, ReplicaMode::Full);
    let engine_nas = nas.start().await;
    engine_nas.connect(engine_phone.addr()).await.unwrap();
    eventually("the NAS has both files", || {
        nas.read("one.txt").is_some() && nas.read("two.txt").is_some()
    })
    .await;
    touch(&engine_phone, "two.txt").await;
    assert!(phone.read("one.txt").is_none(), "one.txt evicted");
    assert!(phone.read("two.txt").is_some());
    assert!(!on_disk(&engine_phone, "one.txt"));

    // An evicted file can be opened again from the NAS.
    assert_eq!(touch(&engine_phone, "one.txt").await, b"first local file");

    engine_nas.shutdown().await;
    engine_phone.shutdown().await;
}
