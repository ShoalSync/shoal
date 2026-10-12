//! Folder roots (ADR 0037): a folder whose root or marker is missing, as when its drive is not
//! mounted, is unavailable instead of having all its files recorded as deleted.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode, Retention};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([37; 32]);

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

    async fn start(&self, user: &UserKey, watch: bool) -> Engine {
        let config = EngineConfig {
            data_dir: self.data.clone(),
            metadata_dir: None,
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch,
            limits: Default::default(),
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            peers: Vec::new(),
            folders: vec![FolderConfig {
                id: FOLDER,
                path: self.root.clone(),
                mode: ReplicaMode::Full,
                members: vec![user.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(1),
                sync: Default::default(),
                confirm_deletes: Default::default(),
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

fn recorded(engine: &Engine, path: &str) -> bool {
    engine.files(FOLDER).unwrap().iter().any(|f| f.path == path)
}

fn unavailable(engine: &Engine) -> Option<String> {
    engine.folders()[0].unavailable.clone()
}

/// Moves `root` away, as unmounting its drive would, leaving `mount_point` behind if set.
fn unmount(root: &Path, mount_point: bool) -> PathBuf {
    let away = root.with_extension("away");
    fs::rename(root, &away).unwrap();
    if mount_point {
        fs::create_dir(root).unwrap();
    }
    away
}

fn mount(root: &Path, away: &Path) {
    let _ = fs::remove_dir(root);
    fs::rename(away, root).unwrap();
}

/// A folder whose drive is not mounted is unavailable: its files are not recorded as deleted,
/// nothing is written to the empty mount point, and it syncs again once the drive is back.
#[tokio::test(flavor = "multi_thread")]
async fn an_unmounted_folder_deletes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([5; 32]);
    let a = Replica::new(dir.path(), "a", 50);
    let b = Replica::new(dir.path(), "b", 51);
    fs::create_dir_all(&a.root).unwrap();
    a.write("photos/one.jpg", b"one");
    a.write("two.txt", b"two");

    let engine_a = a.start(&user, true).await;
    let engine_b = b.start(&user, false).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("a's files reach b", || {
        b.read("photos/one.jpg").is_some() && b.read("two.txt").is_some()
    })
    .await;
    assert!(a.root.join(".tidefell/folder").exists());
    assert_eq!(unavailable(&engine_a), None);

    // Unmounted while running: the watch and later scans find the root gone.
    let away = unmount(&a.root, false);
    eventually("a notices", || unavailable(&engine_a).is_some()).await;
    assert!(!a.root.exists(), "the root must not be created again");
    mount(&a.root, &away);
    eventually("a is available again", || unavailable(&engine_a).is_none()).await;

    // Unmounted across a restart, leaving an empty mount point.
    engine_a.shutdown().await;
    let away = unmount(&a.root, true);
    let engine_a = a.start(&user, true).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    let reason = unavailable(&engine_a).expect("the folder is unavailable");
    assert!(reason.contains("marker"), "{reason}");
    b.write("new.txt", b"from b");
    // Several scans and materialise passes run meanwhile.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(recorded(&engine_a, "photos/one.jpg") && recorded(&engine_a, "two.txt"));
    assert!(b.read("photos/one.jpg").is_some() && b.read("two.txt").is_some());
    assert_eq!(
        fs::read_dir(&a.root).unwrap().count(),
        0,
        "nothing is written"
    );
    assert!(matches!(
        engine_a.scan(FOLDER).await,
        Err(tidefell_engine::Error::FolderUnavailable(..))
    ));

    // Mounted again: syncing resumes, and nothing was lost on either side.
    mount(&a.root, &away);
    eventually("b's new file reaches a", || a.read("new.txt").is_some()).await;
    assert_eq!(unavailable(&engine_a), None);
    assert!(b.read("photos/one.jpg").is_some() && b.read("two.txt").is_some());
    assert_eq!(a.read("two.txt").as_deref(), Some(b"two".as_slice()));
    engine_a.shutdown().await;
    engine_b.shutdown().await;
}

/// A folder from before markers existed is given one if its root holds a file it knows, and
/// stays unavailable if the root holds none of them.
#[tokio::test(flavor = "multi_thread")]
async fn folders_without_a_marker_are_adopted_only_if_recognised() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([6; 32]);
    let a = Replica::new(dir.path(), "a", 60);
    fs::create_dir_all(&a.root).unwrap();
    a.write("kept.txt", b"kept");
    let engine = a.start(&user, false).await;
    eventually("the file is recorded", || recorded(&engine, "kept.txt")).await;
    engine.shutdown().await;

    let marker = a.root.join(".tidefell/folder");
    fs::remove_file(&marker).unwrap();
    let engine = a.start(&user, false).await;
    assert_eq!(unavailable(&engine), None);
    assert!(marker.exists());
    engine.shutdown().await;

    // A different directory at the same path, holding none of the folder's files.
    fs::remove_file(&marker).unwrap();
    let away = unmount(&a.root, true);
    fs::write(a.root.join("other.txt"), b"not ours").unwrap();
    let engine = a.start(&user, false).await;
    assert!(unavailable(&engine).is_some());
    assert!(!marker.exists());
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(recorded(&engine, "kept.txt"));
    assert!(!recorded(&engine, "other.txt"));
    engine.shutdown().await;
    drop(away);
}
