//! End-to-end: two engines with real iroh endpoints on localhost and real folders on disk.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, FolderStatus, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([42; 32]);

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
                // Scans are triggered explicitly so the tests are deterministic.
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

fn status(engine: &Engine) -> FolderStatus {
    engine.status(FOLDER).unwrap()
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
async fn sync_create_edit_delete() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);

    let big = noise(3 * 1024 * 1024, 7);
    a.write("hello.txt", b"hello from a");
    a.write("nested/dir/big.bin", &big);

    let engine_a = a.start(&user).await;
    let engine_b = b.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();

    eventually("initial files on b", || {
        b.read("hello.txt").as_deref() == Some(b"hello from a".as_slice())
            && b.read("nested/dir/big.bin").as_deref() == Some(big.as_slice())
    })
    .await;
    eventually("b has nothing pending", || status(&engine_b).pending == 0).await;
    assert_eq!(status(&engine_a).files, status(&engine_b).files);

    // Edit on b, picked up by b's scan and pushed to a.
    b.write("hello.txt", b"edited on b");
    engine_b.scan(FOLDER).await.unwrap();
    eventually("edit reaches a", || {
        a.read("hello.txt").as_deref() == Some(b"edited on b".as_slice())
    })
    .await;

    // New file on b.
    b.write("from-b.txt", b"new on b");
    engine_b.scan(FOLDER).await.unwrap();
    eventually("new file reaches a", || a.read("from-b.txt").is_some()).await;

    // Delete on a removes the file on b and keeps it in b's recycle bin.
    fs::remove_file(a.root.join("nested/dir/big.bin")).unwrap();
    engine_a.scan(FOLDER).await.unwrap();
    eventually("delete reaches b", || {
        b.read("nested/dir/big.bin").is_none()
    })
    .await;
    eventually("b recycled it", || {
        status(&engine_b).recycled == vec!["nested/dir/big.bin".to_string()]
    })
    .await;
    assert_eq!(status(&engine_a).files, status(&engine_b).files);
    assert!(
        !b.root
            .join(".tidefell/tmp")
            .read_dir()
            .unwrap()
            .any(|_| true),
        "no temp files left"
    );

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn conflicting_edits_converge_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 20);
    let b = Replica::new(dir.path(), "b", 21);
    a.write("doc.txt", b"original");

    let engine_a = a.start(&user).await;
    let engine_b = b.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("initial sync", || b.read("doc.txt").is_some()).await;
    engine_b.shutdown().await;

    // Both sides edit while apart. b's edit is later, so it wins on both replicas.
    a.write("doc.txt", b"edit on a");
    engine_a.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    b.write("doc.txt", b"edit on b, later");

    let engine_b = b.start(&user).await;
    engine_b.scan(FOLDER).await.unwrap();
    assert_eq!(
        status(&engine_b).pending,
        0,
        "restart keeps what was on disk"
    );
    engine_b.connect(engine_a.addr()).await.unwrap();

    eventually("both converge on the later edit", || {
        a.read("doc.txt").as_deref() == Some(b"edit on b, later".as_slice())
            && b.read("doc.txt").as_deref() == Some(b"edit on b, later".as_slice())
    })
    .await;
    eventually("conflict copy recorded on both", || {
        status(&engine_a).conflicts == 1 && status(&engine_b).conflicts == 1
    })
    .await;
    assert_eq!(status(&engine_a).files, status(&engine_b).files);

    // a, whose edit lost, edits the winner. The conflict stays until it is resolved (ADR 0029).
    tokio::time::sleep(Duration::from_millis(20)).await;
    a.write("doc.txt", b"a edits the winner");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("the edit reaches b", || {
        b.read("doc.txt").as_deref() == Some(b"a edits the winner".as_slice())
    })
    .await;
    assert_eq!(status(&engine_a).conflicts, 1);
    assert_eq!(status(&engine_b).conflicts, 1);

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_edit_received_before_local_scan_is_kept() {
    // ADR 0024: b edits a file, a's edit of it arrives before b scans, and b's scan must not
    // supersede a's edit.
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 22);
    let b = Replica::new(dir.path(), "b", 23);
    a.write("doc.txt", b"original");

    let engine_a = a.start(&user).await;
    let engine_b = b.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("initial sync", || b.read("doc.txt").is_some()).await;

    b.write("doc.txt", b"edit on b");
    tokio::time::sleep(Duration::from_millis(20)).await;
    a.write("doc.txt", b"edit on a, longer");
    engine_a.scan(FOLDER).await.unwrap();
    let a_size = b"edit on a, longer".len() as u64;
    eventually("b knows a's edit", || {
        engine_b
            .files(FOLDER)
            .unwrap()
            .iter()
            .any(|f| f.path == "doc.txt" && f.size == a_size)
    })
    .await;
    assert_eq!(
        b.read("doc.txt").as_deref(),
        Some(b"edit on b".as_slice()),
        "b's unrecorded edit is not overwritten"
    );

    engine_b.scan(FOLDER).await.unwrap();
    eventually("the edits conflict on both replicas", || {
        status(&engine_a).conflicts == 1 && status(&engine_b).conflicts == 1
    })
    .await;
    eventually("both replicas agree on disk", || {
        a.read("doc.txt").is_some() && a.read("doc.txt") == b.read("doc.txt")
    })
    .await;

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn files_created_at_the_same_path_are_both_kept() {
    // ADR 0024: two devices create the same path while apart. Both files are kept, and both
    // replicas write the same one to the path and the other under a conflict name.
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 24);
    let b = Replica::new(dir.path(), "b", 25);
    a.write("notes.txt", b"from a");
    b.write("notes.txt", b"from b");

    let engine_a = a.start(&user).await;
    let engine_b = b.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();

    let listing = |dir: &Path| {
        let mut names: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_type().unwrap().is_file())
            .map(|e| {
                (
                    e.file_name().into_string().unwrap(),
                    fs::read(e.path()).unwrap(),
                )
            })
            .collect();
        names.sort();
        names
    };
    eventually("both files on both replicas", || {
        listing(&a.root).len() == 2 && listing(&a.root) == listing(&b.root)
    })
    .await;
    let files = listing(&a.root);
    let contents: Vec<&[u8]> = files.iter().map(|(_, c)| c.as_slice()).collect();
    assert!(contents.contains(&b"from a".as_slice()) && contents.contains(&b"from b".as_slice()));
    assert!(files.iter().any(|(n, _)| n == "notes.txt"));
    assert!(files
        .iter()
        .any(|(n, _)| n.starts_with("notes (conflict ") && n.ends_with(").txt")));

    // Both replicas list the same two paths, and nothing is pending.
    let paths = |e: &Engine| -> Vec<String> {
        e.files(FOLDER)
            .unwrap()
            .into_iter()
            .map(|f| f.path)
            .collect()
    };
    assert_eq!(paths(&engine_a), paths(&engine_b));
    assert_eq!(status(&engine_a).pending, 0);
    assert_eq!(status(&engine_b).pending, 0);

    // A rescan does not turn the layout into new versions.
    let before = engine_a.files(FOLDER).unwrap();
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.scan(FOLDER).await.unwrap();
    assert_eq!(engine_a.files(FOLDER).unwrap().len(), before.len());
    assert_eq!(
        engine_a
            .files(FOLDER)
            .unwrap()
            .iter()
            .map(|f| f.entry)
            .collect::<Vec<_>>(),
        before.iter().map(|f| f.entry).collect::<Vec<_>>()
    );

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}
