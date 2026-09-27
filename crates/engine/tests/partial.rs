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
            local_discovery: false,
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            peers: Vec::new(),
            folders: vec![FolderConfig {
                id: FOLDER,
                path: self.root(),
                mode: self.mode,
                members: vec![self.user.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
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

async fn eventually(what: &str, check: impl FnMut() -> bool) {
    eventually_within(what, Duration::from_secs(20), check).await;
}

async fn eventually_within(what: &str, limit: Duration, mut check: impl FnMut() -> bool) {
    for _ in 0..limit.as_millis() / 20 {
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
async fn files_made_here_are_kept_until_a_full_replica_has_them() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([2; 32]);
    let phone = Device::new(
        tmp.path(),
        "phone",
        30,
        &alice,
        // Fits either file alone but not both, so a background eviction pass keeps the
        // most recently used one.
        ReplicaMode::Partial { budget_bytes: 20 },
    );
    phone.write("one.txt", b"first local file");
    phone.write("two.txt", b"second local file");
    let engine_phone = phone.start().await;
    engine_phone.scan(FOLDER).await.unwrap();

    // Over budget, but no peer holds either file: nothing is evicted.
    touch(&engine_phone, "two.txt").await;
    assert!(phone.read("one.txt").is_some());
    assert!(phone.read("two.txt").is_some());

    // A partial replica holding them does not count (ADR 0022): it may evict them in turn.
    let tablet = Device::new(
        tmp.path(),
        "tablet",
        32,
        &alice,
        ReplicaMode::Partial {
            budget_bytes: 1 << 20,
        },
    );
    let engine_tablet = tablet.start().await;
    engine_tablet.connect(engine_phone.addr()).await.unwrap();
    eventually("the tablet lists both files", || {
        engine_tablet.files(FOLDER).unwrap().len() == 2
    })
    .await;
    touch(&engine_tablet, "one.txt").await;
    touch(&engine_tablet, "two.txt").await;
    // Longer than the engine's confirmation interval.
    tokio::time::sleep(Duration::from_secs(12)).await;
    touch(&engine_phone, "two.txt").await;
    assert!(
        phone.read("one.txt").is_some(),
        "kept while only a partial replica has it"
    );
    assert_eq!(engine_phone.folders()[0].only_here, 2);
    assert!(engine_phone
        .files(FOLDER)
        .unwrap()
        .iter()
        .all(|f| f.only_here));
    assert!(engine_phone
        .files(FOLDER)
        .unwrap()
        .iter()
        .all(|f| f.confirmed_copies == 0));
    engine_tablet.shutdown().await;

    // Once a full replica confirms them, the least recently used one goes.
    let nas = Device::new(tmp.path(), "nas", 31, &alice, ReplicaMode::Full);
    let engine_nas = nas.start().await;
    engine_nas.connect(engine_phone.addr()).await.unwrap();
    eventually("the NAS has both files", || {
        nas.read("one.txt").is_some() && nas.read("two.txt").is_some()
    })
    .await;
    eventually_within(
        "the NAS's copies are confirmed",
        Duration::from_secs(30),
        || {
            engine_phone
                .files(FOLDER)
                .unwrap()
                .iter()
                .all(|f| f.confirmed_copies == 1)
        },
    )
    .await;
    assert_eq!(engine_phone.folders()[0].only_here, 0);
    let copies = engine_phone.copies(FOLDER, "one.txt").unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].device, engine_nas.id());
    assert_eq!(copies[0].name, "test");
    assert!(matches!(
        engine_phone.copies(FOLDER, "missing.txt"),
        Err(Error::NoSuchFile(_))
    ));
    touch(&engine_phone, "two.txt").await;
    assert!(phone.read("one.txt").is_none(), "one.txt evicted");
    assert!(phone.read("two.txt").is_some());
    assert!(!on_disk(&engine_phone, "one.txt"));

    // An evicted file can be opened again from the NAS.
    assert_eq!(touch(&engine_phone, "one.txt").await, b"first local file");

    engine_nas.shutdown().await;
    engine_phone.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_and_rename_work_on_placeholders() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let nas = Device::new(tmp.path(), "nas", 30, &alice, ReplicaMode::Full);
    let laptop = Device::new(
        tmp.path(),
        "laptop",
        31,
        &alice,
        ReplicaMode::Partial {
            budget_bytes: 1 << 20,
        },
    );
    nas.write("a.txt", b"first");
    nas.write("old.txt", b"replaced");
    fs::create_dir_all(nas.root().join("docs")).unwrap();
    nas.write("docs/b.txt", b"bee");
    nas.write("docs/c.txt", b"sea");

    let engine_nas = nas.start().await;
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_laptop = laptop.start().await;
    engine_laptop.connect(engine_nas.addr()).await.unwrap();
    eventually("laptop lists the files", || {
        engine_laptop.files(FOLDER).unwrap().len() == 4
    })
    .await;

    // Renaming a placeholder moves it on the NAS without fetching it here.
    engine_laptop
        .rename(FOLDER, "a.txt", "z.txt")
        .await
        .unwrap();
    assert!(!on_disk(&engine_laptop, "z.txt"));
    eventually("the NAS moves a.txt", || {
        nas.read("z.txt").as_deref() == Some(b"first".as_slice()) && nas.read("a.txt").is_none()
    })
    .await;

    // Renaming a directory moves materialised files on disk and placeholders in metadata.
    assert_eq!(touch(&engine_laptop, "docs/b.txt").await, b"bee");
    engine_laptop
        .rename(FOLDER, "docs", "archive")
        .await
        .unwrap();
    assert_eq!(
        laptop.read("archive/b.txt").as_deref(),
        Some(b"bee".as_slice())
    );
    assert!(laptop.read("docs/b.txt").is_none());
    assert!(on_disk(&engine_laptop, "archive/b.txt"));
    assert!(!on_disk(&engine_laptop, "archive/c.txt"));
    eventually("the NAS moves the directory", || {
        nas.read("archive/b.txt").is_some()
            && nas.read("archive/c.txt").as_deref() == Some(b"sea".as_slice())
            && nas.read("docs/c.txt").is_none()
    })
    .await;

    // Renaming over a file replaces it, and the replaced version goes to the recycle bin.
    engine_laptop
        .rename(FOLDER, "z.txt", "old.txt")
        .await
        .unwrap();
    eventually("the NAS replaces old.txt", || {
        nas.read("old.txt").as_deref() == Some(b"first".as_slice()) && nas.read("z.txt").is_none()
    })
    .await;
    eventually("the replaced file is in the NAS recycle bin", || {
        engine_nas
            .recycle_bin(FOLDER)
            .unwrap()
            .iter()
            .any(|r| r.path == "old.txt")
    })
    .await;

    // Deleting a placeholder deletes it everywhere.
    engine_laptop.delete(FOLDER, "archive/c.txt").await.unwrap();
    assert!(engine_laptop
        .files(FOLDER)
        .unwrap()
        .iter()
        .all(|f| f.path != "archive/c.txt"));
    eventually("the NAS deletes archive/c.txt", || {
        nas.read("archive/c.txt").is_none()
    })
    .await;

    // Deleting a materialised file removes it from disk here too.
    engine_laptop.delete(FOLDER, "archive/b.txt").await.unwrap();
    assert!(laptop.read("archive/b.txt").is_none());
    eventually("the NAS deletes archive/b.txt", || {
        nas.read("archive/b.txt").is_none()
    })
    .await;

    assert!(matches!(
        engine_laptop.delete(FOLDER, "nope.txt").await,
        Err(Error::NoSuchFile(_))
    ));
    assert!(matches!(
        engine_laptop.rename(FOLDER, "nope", "elsewhere").await,
        Err(Error::NoSuchFile(_))
    ));
    assert!(matches!(
        engine_laptop
            .rename(FOLDER, "old.txt", "old.txt/inside")
            .await,
        Err(Error::Invalid(_))
    ));

    engine_laptop.shutdown().await;
    engine_nas.shutdown().await;
}
