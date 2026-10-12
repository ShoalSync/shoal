//! Rebuilding a folder whose metadata is damaged or lost (ADR 0034): the engine starts with its
//! other folders, the folder matches its files on disk to what peers send, gets its recycle bin
//! and history back from sidecars, and nothing is duplicated.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, FolderState, PeerAddr, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::{DeviceId, FolderId};

const DAMAGED: FolderId = FolderId([36; 32]);
const OTHER: FolderId = FolderId([37; 32]);

struct Replica {
    dir: PathBuf,
    key: DeviceKey,
}

impl Replica {
    fn new(dir: &Path, name: &str, seed: u8) -> Self {
        Self {
            dir: dir.join(name),
            key: DeviceKey::from_seed([seed; 32]),
        }
    }

    fn root(&self, folder: FolderId) -> PathBuf {
        self.dir.join(if folder == DAMAGED {
            "damaged"
        } else {
            "other"
        })
    }

    fn database(&self, folder: FolderId) -> PathBuf {
        self.dir
            .join("data/metadata")
            .join(format!("{folder}.redb"))
    }

    /// Starts the engine; `peer` is a device to wait for while a folder is rebuilt.
    async fn start(&self, user: &UserKey, peer: Option<DeviceId>) -> Engine {
        let folder = |id| FolderConfig {
            id,
            path: self.root(id),
            mode: ReplicaMode::Full,
            members: vec![user.id()],
            retention: Retention::Days(30),
            // Scans are triggered explicitly, or by a rebuild.
            scan_interval: Duration::from_secs(3600),
            sync: Default::default(),
        };
        let config = EngineConfig {
            data_dir: self.dir.join("data"),
            metadata_dir: None,
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            limits: Default::default(),
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            // Known, with no address to dial: the tests connect themselves.
            peers: peer
                .map(|device| PeerAddr {
                    device,
                    addrs: Vec::new(),
                    relay: None,
                })
                .into_iter()
                .collect(),
            folders: vec![folder(DAMAGED), folder(OTHER)],
        };
        let certificate = user.certify(self.key.id(), "test", 0);
        Engine::start(config, self.key.clone(), certificate)
            .await
            .unwrap()
    }

    fn write(&self, folder: FolderId, path: &str, data: &[u8]) {
        let abs = self.root(folder).join(path);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, data).unwrap();
    }

    fn read(&self, folder: FolderId, path: &str) -> Option<Vec<u8>> {
        fs::read(self.root(folder).join(path)).ok()
    }

    /// The files at the top of the folder, as they are on disk.
    fn names(&self, folder: FolderId) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.root(folder))
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_file())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
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

fn paths(engine: &Engine, folder: FolderId) -> Vec<String> {
    let mut paths: Vec<String> = engine
        .files(folder)
        .unwrap()
        .into_iter()
        .map(|f| f.path)
        .collect();
    paths.sort();
    paths
}

fn info(engine: &Engine, folder: FolderId) -> tidefell_engine::FolderInfo {
    engine
        .folders()
        .into_iter()
        .find(|f| f.id == folder)
        .unwrap()
}

fn rebuilt(engine: &Engine, folder: FolderId) -> bool {
    info(engine, folder)
        .rebuild
        .is_some_and(|r| r.finished_ms.is_some())
}

fn recycled(engine: &Engine, folder: FolderId) -> Vec<String> {
    let mut paths: Vec<String> = engine
        .recycle_bin(folder)
        .unwrap()
        .into_iter()
        .map(|r| r.path)
        .collect();
    paths.sort();
    paths
}

fn previous_versions(engine: &Engine, folder: FolderId, path: &str) -> usize {
    engine
        .versions(folder, path)
        .unwrap()
        .iter()
        .filter(|v| v.id.starts_with("previous:"))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_damaged_folder_is_rebuilt_while_the_others_sync() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    b.write(DAMAGED, "keep.txt", b"on both");
    b.write(DAMAGED, "gone.txt", b"deleted on b");
    b.write(DAMAGED, "edited.txt", b"first");
    b.write(OTHER, "one.txt", b"in the other folder");
    let engine_b = b.start(&user, None).await;
    engine_b.scan(DAMAGED).await.unwrap();
    engine_b.scan(OTHER).await.unwrap();
    let engine_a = a.start(&user, Some(b.key.id())).await;
    a.write(DAMAGED, "mine.txt", b"made on a");
    engine_a.scan(DAMAGED).await.unwrap();
    engine_a.connect(engine_b.addr()).await.unwrap();
    eventually("the folders sync", || {
        a.names(DAMAGED).len() == 4 && b.names(DAMAGED).len() == 4 && a.names(OTHER).len() == 1
    })
    .await;

    // Deleted and edited on b: a keeps one for its recycle bin, the other in its history.
    fs::remove_file(b.root(DAMAGED).join("gone.txt")).unwrap();
    b.write(DAMAGED, "edited.txt", b"second, longer");
    engine_b.scan(DAMAGED).await.unwrap();
    eventually("the changes reach a", || {
        a.read(DAMAGED, "gone.txt").is_none()
            && a.read(DAMAGED, "edited.txt").as_deref() == Some(b"second, longer".as_slice())
    })
    .await;
    assert_eq!(recycled(&engine_a, DAMAGED), ["gone.txt"]);
    assert_eq!(previous_versions(&engine_a, DAMAGED, "edited.txt"), 1);
    engine_a.shutdown().await;

    // The database is damaged, and a file is made while a is stopped.
    let database = a.database(DAMAGED);
    let length = fs::metadata(&database).unwrap().len() as usize;
    fs::write(&database, vec![0xab; length]).unwrap();
    a.write(DAMAGED, "offline.txt", b"made while stopped");

    // a starts, with the other folder as it was and the damaged one rebuilding.
    let engine_a = a.start(&user, Some(b.key.id())).await;
    assert_eq!(paths(&engine_a, OTHER), ["one.txt"]);
    let damaged = info(&engine_a, DAMAGED);
    let rebuild = damaged.rebuild.expect("the folder is rebuilt");
    assert!(rebuild.reason.contains("damaged"), "{}", rebuild.reason);
    assert!(
        fs::read_dir(a.dir.join("data/metadata"))
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".corrupt-")),
        "the damaged database is kept aside"
    );
    // Nothing is recorded or deleted before a peer's index is here.
    engine_a.scan(DAMAGED).await.unwrap();
    assert!(!rebuilt(&engine_a, DAMAGED));
    assert_eq!(info(&engine_a, DAMAGED).state, FolderState::Rebuilding);
    assert_eq!(
        a.names(DAMAGED),
        ["edited.txt", "keep.txt", "mine.txt", "offline.txt"]
    );
    // The recycle bin is back already; history shows once its file's entry is.
    assert_eq!(recycled(&engine_a, DAMAGED), ["gone.txt"]);

    engine_a.connect(engine_b.addr()).await.unwrap();
    // The other folder syncs as before.
    b.write(OTHER, "two.txt", b"after the restart");
    engine_b.scan(OTHER).await.unwrap();
    eventually("the other folder syncs", || {
        a.read(OTHER, "two.txt").is_some()
    })
    .await;

    // b's index arrives: files on disk are matched, and the file made while stopped is new.
    eventually("the rebuild finishes", || rebuilt(&engine_a, DAMAGED)).await;
    eventually("the new file reaches b", || {
        b.read(DAMAGED, "offline.txt").is_some()
    })
    .await;
    let expected = ["edited.txt", "keep.txt", "mine.txt", "offline.txt"];
    eventually("both have every file once", || {
        paths(&engine_a, DAMAGED) == expected && paths(&engine_b, DAMAGED) == expected
    })
    .await;
    assert_eq!(a.names(DAMAGED), expected, "nothing duplicated on disk");
    assert_eq!(b.names(DAMAGED), expected);
    assert_eq!(info(&engine_a, DAMAGED).conflicts, 0);
    assert_eq!(info(&engine_b, DAMAGED).conflicts, 0);
    assert_ne!(info(&engine_a, DAMAGED).state, FolderState::Rebuilding);
    assert_eq!(recycled(&engine_a, DAMAGED), ["gone.txt"]);
    assert_eq!(previous_versions(&engine_a, DAMAGED, "edited.txt"), 1);
    // Edits made here after the rebuild supersede this device's versions from before it.
    a.write(DAMAGED, "mine.txt", b"edited on a after the rebuild");
    a.write(DAMAGED, "edited.txt", b"third");
    engine_a.scan(DAMAGED).await.unwrap();
    eventually("the edits reach b", || {
        b.read(DAMAGED, "mine.txt").as_deref() == Some(b"edited on a after the rebuild".as_slice())
            && b.read(DAMAGED, "edited.txt").as_deref() == Some(b"third".as_slice())
    })
    .await;
    assert_eq!(info(&engine_b, DAMAGED).conflicts, 0);
    assert_eq!(info(&engine_a, DAMAGED).conflicts, 0);
    // The rebuilt folder serves what it kept: b has emptied its own recycle bin, so restoring
    // a deleted file and a previous version fetches their content from a.
    engine_b.empty_recycle_bin(DAMAGED, None).unwrap();
    let first = engine_a
        .versions(DAMAGED, "edited.txt")
        .unwrap()
        .into_iter()
        .filter(|v| v.id.starts_with("previous:"))
        .find(|v| v.size == 5)
        .expect("the first version is in a's history");
    engine_a
        .restore_version(DAMAGED, "edited.txt", &first.id)
        .await
        .unwrap();
    eventually("the previous version reaches b", || {
        b.read(DAMAGED, "edited.txt").as_deref() == Some(b"first".as_slice())
    })
    .await;
    // Restoring from the recycle bin works on the rebuilt folder.
    let gone = engine_a
        .recycle_bin(DAMAGED)
        .unwrap()
        .into_iter()
        .find(|r| r.path == "gone.txt")
        .unwrap();
    engine_a.restore(DAMAGED, gone.entry).await.unwrap();
    eventually("the restored file reaches b", || {
        b.read(DAMAGED, "gone.txt").as_deref() == Some(b"deleted on b".as_slice())
    })
    .await;

    engine_a.shutdown().await;
    engine_b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rebuild_with_no_peer_ends_at_the_first_scan() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    a.write(DAMAGED, "kept.txt", b"stays");
    a.write(DAMAGED, "gone.txt", b"deleted");
    let engine = a.start(&user, None).await;
    engine.scan(DAMAGED).await.unwrap();
    engine.delete(DAMAGED, "gone.txt").await.unwrap();
    assert_eq!(recycled(&engine, DAMAGED), ["gone.txt"]);
    engine.rebuild_folder(DAMAGED).await.unwrap();
    eventually("the rebuild finishes", || rebuilt(&engine, DAMAGED)).await;
    eventually("the files are recorded again", || {
        paths(&engine, DAMAGED) == ["kept.txt"]
    })
    .await;
    assert_eq!(a.names(DAMAGED), ["kept.txt"]);
    assert_eq!(recycled(&engine, DAMAGED), ["gone.txt"]);
    let rebuild = info(&engine, DAMAGED).rebuild.unwrap();
    assert_eq!(rebuild.reason, "it was asked for");
    // The check of a database in use passes, and leaves it working.
    let checks = engine.doctor();
    assert!(
        checks
            .iter()
            .filter(|c| c.name == "metadata")
            .all(|c| c.level == tidefell_engine::CheckLevel::Pass),
        "{checks:?}"
    );
    engine.delete(DAMAGED, "kept.txt").await.unwrap();
    assert_eq!(paths(&engine, DAMAGED), Vec::<String>::new());
    engine.shutdown().await;

    // A lost database is a rebuild too, not a new folder.
    for entry in fs::read_dir(a.dir.join("data/metadata")).unwrap().flatten() {
        fs::remove_file(entry.path()).unwrap();
    }
    let engine = a.start(&user, None).await;
    let rebuild = info(&engine, DAMAGED).rebuild.unwrap();
    assert!(rebuild.reason.contains("missing"), "{}", rebuild.reason);
    eventually("the rebuild finishes", || rebuilt(&engine, DAMAGED)).await;
    eventually("the rebuild settles", || paths(&engine, DAMAGED).is_empty()).await;
    assert_eq!(recycled(&engine, DAMAGED), ["gone.txt", "kept.txt"]);
    engine.shutdown().await;
}
