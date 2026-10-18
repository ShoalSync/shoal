//! Ignore patterns (ADR 0029): an ignored path never leaves its device, other devices'
//! versions of it are listed but not written, and a path that becomes ignored keeps its entry.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, FolderStatus, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([30; 32]);

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
                confirm_deletes: Default::default(),
                encryption: None,
            }],
            min_free_space: None,
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

fn status(engine: &Engine) -> FolderStatus {
    engine.status(FOLDER).unwrap()
}

fn listed(engine: &Engine, path: &str) -> Option<bool> {
    engine
        .files(FOLDER)
        .unwrap()
        .into_iter()
        .find(|f| f.path == path)
        .map(|f| f.on_disk)
}

#[tokio::test(flavor = "multi_thread")]
async fn ignored_paths_stay_on_their_device() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([3; 32]);
    let a = Replica::new(dir.path(), "a", 30);
    let b = Replica::new(dir.path(), "b", 31);

    a.write(".tidefellignore", b"*.log\nbuild/\n");
    a.write("keep.txt", b"synced");
    a.write("debug.log", b"only on a");
    a.write("build/out/app.bin", b"only on a");
    a.write(".DS_Store", b"finder junk");
    a.write("video.mp4.part", b"half a download");

    let engine_a = a.start(&user).await;
    let engine_b = b.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();

    eventually("keep.txt reaches b", || b.read("keep.txt").is_some()).await;
    // Nothing ignored on a is recorded, so nothing of it reaches b.
    for path in [
        ".tidefellignore",
        "debug.log",
        "build/out/app.bin",
        ".DS_Store",
        "video.mp4.part",
    ] {
        assert_eq!(listed(&engine_a, path), None, "{path} was recorded on a");
        assert_eq!(listed(&engine_b, path), None, "{path} reached b");
        assert!(b.read(path).is_none());
    }

    // B does not ignore logs: its log is listed on a but never written there.
    b.write("from-b.log", b"made on b");
    engine_b.scan(FOLDER).await.unwrap();
    eventually("a lists b's log", || {
        listed(&engine_a, "from-b.log").is_some()
    })
    .await;
    engine_a.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(listed(&engine_a, "from-b.log"), Some(false));
    assert!(a.read("from-b.log").is_none());
    assert_eq!(status(&engine_a).pending, 0);

    // A path that becomes ignored after it synced keeps its entry: a's later edits are not
    // sent, and b's delete is not applied on a.
    a.write("notes.txt", b"first");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("notes.txt reaches b", || {
        b.read("notes.txt").as_deref() == Some(b"first".as_slice())
    })
    .await;
    a.write(".tidefellignore", b"*.log\nbuild/\nnotes.txt\n");
    a.write("notes.txt", b"second, private");
    engine_a.scan(FOLDER).await.unwrap();
    fs::remove_file(b.root.join("notes.txt")).unwrap();
    engine_b.scan(FOLDER).await.unwrap();
    eventually("a hears of b's delete", || {
        listed(&engine_a, "notes.txt").is_none()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        a.read("notes.txt").as_deref(),
        Some(b"second, private".as_slice())
    );
    assert!(b.read("notes.txt").is_none());

    // A file saved through the engine, as a mounted folder or a web upload does, is an explicit
    // save: it is recorded even at an ignored path, so a mount still shows it.
    let source = dir.path().join("download");
    fs::write(&source, b"downloaded").unwrap();
    engine_a
        .write(FOLDER, "film.mkv.part", &source)
        .await
        .unwrap();
    assert_eq!(listed(&engine_a, "film.mkv.part"), Some(true));
    engine_a
        .rename(FOLDER, "film.mkv.part", "film.mkv")
        .await
        .unwrap();
    eventually("the finished download reaches b", || {
        b.read("film.mkv").as_deref() == Some(b"downloaded".as_slice())
    })
    .await;
    assert!(a.read("film.mkv.part").is_none());
    // The ignore file itself is never recorded, however it is written.
    let source = dir.path().join("ignore");
    fs::write(&source, b"*.log\n").unwrap();
    engine_a
        .write(FOLDER, ".tidefellignore", &source)
        .await
        .unwrap();
    assert_eq!(listed(&engine_a, ".tidefellignore"), None);
}
