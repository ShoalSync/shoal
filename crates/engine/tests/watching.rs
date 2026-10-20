//! Watching for changes (ADR 0029): with a one-hour scan interval, changes are still picked up
//! within seconds, and only where they happened.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tidefell_engine::{Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode, Retention};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([31; 32]);

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
        fs::create_dir_all(&self.root).unwrap();
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
                // Without the watch, nothing would be noticed during the test.
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

fn recorded(engine: &Engine, path: &str) -> bool {
    engine.files(FOLDER).unwrap().iter().any(|f| f.path == path)
}

#[tokio::test(flavor = "multi_thread")]
async fn changes_are_picked_up_without_scanning() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([4; 32]);
    let a = Replica::new(dir.path(), "a", 40);
    let b = Replica::new(dir.path(), "b", 41);
    a.write("docs/old.txt", b"before");
    a.write("album/one.jpg", b"one");
    a.write("album/two.jpg", b"two");

    let engine_a = a.start(&user, true).await;
    let engine_b = b.start(&user, false).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("the first scan reaches b", || {
        b.read("album/two.jpg").is_some()
    })
    .await;
    assert!(engine_a.folders()[0].watch_error.is_none());

    // A new file in a subdirectory is recorded within two seconds.
    let start = Instant::now();
    a.write("docs/new.txt", b"after");
    eventually("the new file is recorded", || {
        recorded(&engine_a, "docs/new.txt")
    })
    .await;
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "took {:?}",
        start.elapsed()
    );
    eventually("it reaches b", || b.read("docs/new.txt").is_some()).await;

    // An edit and a delete in a subdirectory.
    a.write("docs/new.txt", b"edited");
    fs::remove_file(a.root.join("docs/old.txt")).unwrap();
    eventually("the edit reaches b", || {
        b.read("docs/new.txt").as_deref() == Some(b"edited".as_slice())
    })
    .await;
    eventually("the delete reaches b", || b.read("docs/old.txt").is_none()).await;

    // A watched directory renamed, and a file added under its new name.
    fs::rename(a.root.join("docs"), a.root.join("papers")).unwrap();
    a.write("papers/added.txt", b"added");
    eventually("the renamed directory's files reach b", || {
        b.read("papers/new.txt").as_deref() == Some(b"edited".as_slice())
            && b.read("papers/added.txt").is_some()
            && b.read("docs/new.txt").is_none()
    })
    .await;

    // A whole directory removed.
    fs::remove_dir_all(a.root.join("album")).unwrap();
    eventually("the removed directory's files leave b", || {
        b.read("album/one.jpg").is_none() && b.read("album/two.jpg").is_none()
    })
    .await;

    // A directory moved in from outside the folder, with files already in it.
    let outside = dir.path().join("outside");
    fs::create_dir_all(outside.join("deep")).unwrap();
    fs::write(outside.join("deep/x.txt"), b"moved in").unwrap();
    fs::rename(&outside, a.root.join("moved")).unwrap();
    eventually("the moved-in files reach b", || {
        b.read("moved/deep/x.txt").as_deref() == Some(b"moved in".as_slice())
    })
    .await;
    // And a file added in it afterwards, which needs the new directory's watch.
    a.write("moved/deep/later.txt", b"later");
    eventually("a file in the moved-in directory reaches b", || {
        b.read("moved/deep/later.txt").is_some()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_written_continuously_is_still_picked_up() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([5; 32]);
    let a = Replica::new(dir.path(), "a", 50);
    let engine = a.start(&user, true).await;

    // Written every 200 ms for 8 s: changes never settle for a second, but the scan runs anyway
    // once they have waited five.
    let path = a.root.join("log.txt");
    let writer = tokio::spawn(async move {
        let mut text = String::new();
        for i in 0..40 {
            text.push_str(&format!("line {i}\n"));
            fs::write(&path, &text).unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });
    eventually("the log is recorded while it is still written", || {
        recorded(&engine, "log.txt")
    })
    .await;
    assert!(!writer.is_finished(), "only recorded once writing stopped");
    writer.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn without_watching_nothing_is_noticed() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([6; 32]);
    let a = Replica::new(dir.path(), "a", 60);
    let engine = a.start(&user, false).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.write("new.txt", b"unseen");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!recorded(&engine, "new.txt"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_past_windows_old_path_limit_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([7; 32]);
    let a = Replica::new(dir.path(), "a", 70);
    let b = Replica::new(dir.path(), "b", 71);
    // Longer than the 260 characters Windows programs were once limited to (ADR 0042).
    let deep = format!("{}/notes.txt", vec!["d".repeat(60); 5].join("/"));
    assert!(a.root.join(&deep).as_os_str().len() > 300);

    let engine_a = a.start(&user, true).await;
    let engine_b = b.start(&user, false).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    a.write(&deep, b"deep");
    eventually("the deep file reaches b", || {
        b.read(&deep).as_deref() == Some(b"deep".as_slice())
    })
    .await;
    a.write(&deep, b"deeper");
    eventually("its edit reaches b", || {
        b.read(&deep).as_deref() == Some(b"deeper".as_slice())
    })
    .await;
}

/// A file another program holds open without sharing, as some Windows editors do, gets its
/// newer version once that program closes it; until then it is a problem tried again, and
/// what was fetched for it is not left behind (ADR 0042).
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread")]
async fn a_file_held_open_gets_its_update_once_closed() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([8; 32]);
    let a = Replica::new(dir.path(), "a", 80);
    let b = Replica::new(dir.path(), "b", 81);
    a.write("held.txt", b"one");
    let engine_a = a.start(&user, true).await;
    let engine_b = b.start(&user, false).await;
    engine_b.set_retry_interval(Duration::from_millis(200));
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("the file reaches b", || {
        b.read("held.txt").as_deref() == Some(b"one".as_slice())
    })
    .await;

    let held = fs::File::options()
        .read(true)
        .share_mode(0)
        .open(b.root.join("held.txt"))
        .unwrap();
    a.write("held.txt", b"two");
    eventually("b tries it again, as a file in use", || {
        engine_b.path_problems(FOLDER).unwrap().iter().any(|p| {
            p.path == "held.txt" && p.message.contains("open in another program") && p.attempts >= 2
        })
    })
    .await;
    let tmp = b.root.join(".tidefell").join("tmp");
    eventually("nothing fetched is left behind", || {
        fs::read_dir(&tmp).map_or(true, |d| d.count() == 0)
    })
    .await;

    drop(held);
    eventually("the update lands once the file is closed", || {
        b.read("held.txt").as_deref() == Some(b"two".as_slice())
    })
    .await;
    eventually("and the problem goes", || {
        engine_b.path_problems(FOLDER).unwrap().is_empty()
    })
    .await;
    // The version replaced is kept once, beside its record, however many times its
    // replacement was tried.
    let history = b.root.join(".tidefell").join("history");
    let kept: Vec<_> = fs::read_dir(&history)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_none_or(|e| e != "json"))
        .collect();
    assert_eq!(kept.len(), 1, "kept {kept:?}");
    assert_eq!(fs::read(&kept[0]).unwrap(), b"one");
}

/// A burst of changes larger than a watch's buffer still leaves the folder watched: the watch
/// asks for a full scan rather than stopping (ADR 0042).
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_changes_leaves_the_folder_watched() {
    const FILES: usize = 3000;
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([9; 32]);
    let a = Replica::new(dir.path(), "a", 90);
    a.write("first.txt", b"first");
    let engine = a.start(&user, true).await;
    eventually("the first scan", || recorded(&engine, "first.txt")).await;

    for i in 0..FILES {
        a.write(&format!("burst/{i:04}.txt"), format!("file {i}").as_bytes());
    }
    let mut waited = 0;
    while engine.files(FOLDER).unwrap().len() < FILES + 1 && waited < 600 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        waited += 1;
    }
    assert_eq!(
        engine.files(FOLDER).unwrap().len(),
        FILES + 1,
        "every file recorded"
    );

    let start = Instant::now();
    a.write("after.txt", b"after");
    eventually("a change after the burst is still noticed", || {
        recorded(&engine, "after.txt")
    })
    .await;
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
}
