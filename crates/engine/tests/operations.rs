//! Runtime operations: folders added and joined while running, membership changes, the
//! recycle bin, restarts and events.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, Error, Event, FolderConfig, NewFolder, RelayConfig, ReplicaMode,
    Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::{FolderId, UserId};

const FOLDER: FolderId = FolderId([7; 32]);

struct Device {
    dir: PathBuf,
    key: DeviceKey,
    user: UserKey,
}

impl Device {
    fn new(root: &Path, name: &str, seed: u8, user: &UserKey) -> Self {
        Self {
            dir: root.join(name),
            key: DeviceKey::from_seed([seed; 32]),
            user: user.clone(),
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.join("folder")
    }

    async fn start(&self, configured: Vec<FolderConfig>) -> Engine {
        let config = EngineConfig {
            data_dir: self.dir.join("data"),
            metadata_dir: None,
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            limits: Default::default(),
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            peers: Vec::new(),
            folders: configured,
            min_free_space: None,
        };
        let certificate = self.user.certify(self.key.id(), "test", 0);
        Engine::start(config, self.key.clone(), certificate)
            .await
            .unwrap()
    }

    fn folder(&self, members: &[UserId]) -> FolderConfig {
        FolderConfig {
            id: FOLDER,
            path: self.root(),
            mode: ReplicaMode::Full,
            members: members.to_vec(),
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(3600),
            sync: Default::default(),
            confirm_deletes: Default::default(),
        }
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.root().join(path)).ok()
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

#[tokio::test(flavor = "multi_thread")]
async fn share_join_delete_restore_and_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let a = Device::new(tmp.path(), "a", 10, &alice);
    let b = Device::new(tmp.path(), "b", 11, &alice);

    let engine_a = a.start(Vec::new()).await;
    let engine_b = b.start(Vec::new()).await;
    // The session starts with nothing to share.
    engine_b.connect(engine_a.addr()).await.unwrap();

    // a creates the folder, then b joins it over the existing session.
    engine_a.add_folder(a.folder(&[alice.id()])).unwrap();
    fs::write(a.root().join("notes.txt"), b"shared notes").unwrap();
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.add_folder(b.folder(&[alice.id()])).unwrap();
    assert!(matches!(
        engine_b.add_folder(b.folder(&[alice.id()])),
        Err(Error::FolderExists(_))
    ));

    eventually("file reaches b after joining", || {
        b.read("notes.txt").as_deref() == Some(b"shared notes".as_slice())
    })
    .await;
    let files = engine_b.files(FOLDER).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "notes.txt");
    assert!(files[0].on_disk);
    assert_eq!(
        engine_b.local_path(FOLDER, "notes.txt").unwrap(),
        Some(b.root().join("notes.txt"))
    );
    let peers = engine_b.connected_peers();
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].shared_folders, vec![FOLDER]);

    // Delete on a; b removes the file and keeps it in the recycle bin.
    fs::remove_file(a.root().join("notes.txt")).unwrap();
    engine_a.scan(FOLDER).await.unwrap();
    eventually("delete reaches b", || b.read("notes.txt").is_none()).await;
    eventually("b recycled it", || {
        engine_b.recycle_bin(FOLDER).unwrap().len() == 1
    })
    .await;
    let recycled = engine_b.recycle_bin(FOLDER).unwrap().remove(0);
    assert_eq!(recycled.path, "notes.txt");
    assert_eq!(recycled.size, 12);
    assert!(recycled.expires_at_ms.is_some());

    // Restore on b brings it back on both devices.
    engine_b.restore(FOLDER, recycled.entry).await.unwrap();
    eventually("restored on b", || {
        b.read("notes.txt").as_deref() == Some(b"shared notes".as_slice())
    })
    .await;
    eventually("restored on a", || {
        a.read("notes.txt").as_deref() == Some(b"shared notes".as_slice())
    })
    .await;
    assert!(engine_b.recycle_bin(FOLDER).unwrap().is_empty());

    // Runtime folders survive a restart.
    engine_b.shutdown().await;
    let engine_b = b.start(Vec::new()).await;
    let folders = engine_b.folders();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].id, FOLDER);
    assert!(!folders[0].configured);
    assert_eq!(engine_b.status(FOLDER).unwrap().pending, 0);

    // Removing it stops syncing and forgets metadata, but leaves files alone.
    engine_b.remove_folder(FOLDER).unwrap();
    assert!(engine_b.folders().is_empty());
    assert!(b.read("notes.txt").is_some());
    engine_b.shutdown().await;
    assert!(b.start(Vec::new()).await.folders().is_empty());

    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_and_emptying_the_recycle_bin() {
    const DAY_MS: i64 = 86_400_000;
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let a = Device::new(tmp.path(), "a", 40, &alice);
    let engine = a.start(Vec::new()).await;
    engine.add_folder(a.folder(&[alice.id()])).unwrap();

    let delete = |names: &[&str]| {
        for name in names {
            fs::remove_file(a.root().join(name)).unwrap();
        }
    };
    for (name, body) in [("a.txt", "aaaa"), ("b.txt", "bbbbbb"), ("c.txt", "cc")] {
        fs::write(a.root().join(name), body).unwrap();
    }
    engine.scan(FOLDER).await.unwrap();
    delete(&["a.txt", "b.txt", "c.txt"]);
    engine.scan(FOLDER).await.unwrap();

    let summary = engine.folders()[0].recycle_bin;
    assert_eq!(summary.items, 3);
    assert_eq!(summary.bytes, 12);
    let bin = engine.recycle_bin(FOLDER).unwrap();
    let first_expiry = bin.iter().filter_map(|r| r.expires_at_ms).min();
    assert_eq!(summary.next_expiry_ms, first_expiry);

    // One item, then the rest.
    let a_txt = bin.iter().find(|r| r.path == "a.txt").unwrap().entry;
    let emptied = engine.empty_recycle_bin(FOLDER, Some(a_txt)).unwrap();
    assert_eq!(emptied.len(), 1);
    assert_eq!(emptied[0].path, "a.txt");
    assert!(matches!(
        engine.empty_recycle_bin(FOLDER, Some(a_txt)),
        Err(Error::NotRecycled(_))
    ));
    assert!(engine.restore(FOLDER, a_txt).await.is_err());
    assert_eq!(engine.empty_recycle_bin(FOLDER, None).unwrap().len(), 2);
    assert_eq!(engine.folders()[0].recycle_bin, Default::default());

    // Retention changes are kept across restarts.
    fs::write(a.root().join("d.txt"), "dddd").unwrap();
    engine.scan(FOLDER).await.unwrap();
    delete(&["d.txt"]);
    engine.scan(FOLDER).await.unwrap();
    engine.set_retention(FOLDER, Retention::Forever).unwrap();
    assert_eq!(engine.folders()[0].retention, Retention::Forever);
    assert_eq!(engine.folders()[0].recycle_bin.next_expiry_ms, None);
    engine.shutdown().await;
    let engine = a.start(Vec::new()).await;
    let folder = &engine.folders()[0];
    assert_eq!(folder.retention, Retention::Forever);
    assert_eq!(folder.recycle_bin.items, 1);

    // Shortening retention purges at the next purge, not before.
    engine.set_retention(FOLDER, Retention::Days(1)).unwrap();
    assert_eq!(engine.folders()[0].recycle_bin.items, 1);
    let deleted_at = engine.recycle_bin(FOLDER).unwrap()[0].deleted_at_ms;
    engine.purge(deleted_at + DAY_MS / 2);
    assert_eq!(engine.folders()[0].recycle_bin.items, 1);
    engine.purge(deleted_at + DAY_MS);
    assert_eq!(engine.folders()[0].recycle_bin.items, 0);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn membership_granted_at_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let bob = UserKey::from_seed([2; 32]);
    let a = Device::new(tmp.path(), "a", 20, &alice);
    let b = Device::new(tmp.path(), "b", 21, &bob);

    let engine_a = a.start(Vec::new()).await;
    let engine_b = b.start(Vec::new()).await;
    engine_a.add_folder(a.folder(&[alice.id()])).unwrap();
    engine_b
        .add_folder(b.folder(&[alice.id(), bob.id()]))
        .unwrap();
    fs::write(a.root().join("for-bob.txt"), b"hello bob").unwrap();
    engine_a.scan(FOLDER).await.unwrap();

    let mut events = engine_b.events();
    engine_b.connect(engine_a.addr()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(b.read("for-bob.txt").is_none(), "bob is not a member yet");

    assert!(engine_a.add_member(FOLDER, bob.id()).unwrap());
    assert!(!engine_a.add_member(FOLDER, bob.id()).unwrap());
    eventually("bob receives the file once admitted", || {
        b.read("for-bob.txt").as_deref() == Some(b"hello bob".as_slice())
    })
    .await;

    // The file is in place a moment before the event for it is emitted.
    let updated = Event::FileUpdated {
        folder: FOLDER,
        path: "for-bob.txt".into(),
    };
    let saw_update = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Ok(event) if event == updated => return true,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(saw_update, "a FileUpdated event was emitted");

    // The last member cannot be removed.
    assert!(engine_a.remove_member(FOLDER, bob.id()).unwrap());
    assert!(matches!(
        engine_a.remove_member(FOLDER, alice.id()),
        Err(Error::Invalid(_))
    ));

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn configured_folders_are_read_only_at_runtime() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let a = Device::new(tmp.path(), "a", 30, &alice);
    let engine = a.start(vec![a.folder(&[alice.id()])]).await;
    assert!(engine.folders()[0].configured);
    assert!(matches!(
        engine.remove_folder(FOLDER),
        Err(Error::Configured(_))
    ));
    assert!(matches!(
        engine.add_member(FOLDER, UserKey::from_seed([2; 32]).id()),
        Err(Error::Configured(_))
    ));
    assert!(matches!(
        engine.set_retention(FOLDER, Retention::Forever),
        Err(Error::Configured(_))
    ));
    engine.shutdown().await;
}

#[tokio::test]
async fn created_folders_get_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let bob = UserKey::from_seed([2; 32]);
    let a = Device::new(tmp.path(), "a", 40, &alice);
    let engine = a.start(Vec::new()).await;
    assert_eq!(engine.user(), alice.id());
    let new = |id, path: PathBuf, members: Vec<UserId>| NewFolder {
        id,
        path,
        mode: ReplicaMode::from_budget(Some(1000)),
        members,
        retention: Retention::from_days(0),
        scan_interval: Duration::from_secs(3600),
        sync: Default::default(),
    };

    // A new folder gets a fresh ID and this device's user as its only member.
    let first = engine
        .create_folder(new(None, a.root().join("one"), Vec::new()))
        .unwrap();
    let second = engine
        .create_folder(new(None, a.root().join("two"), Vec::new()))
        .unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(first.members, vec![alice.id()]);
    assert_eq!(first.mode, ReplicaMode::Partial { budget_bytes: 1000 });
    assert_eq!(first.mode.budget_bytes(), Some(1000));
    assert_eq!(ReplicaMode::from_budget(None).budget_bytes(), None);
    assert_eq!(first.retention, Retention::Forever);

    // Joining keeps the given ID and members.
    let joined = engine
        .create_folder(new(Some(FOLDER), a.root().join("three"), vec![bob.id()]))
        .unwrap();
    assert_eq!((joined.id, joined.members), (FOLDER, vec![bob.id()]));
    assert!(matches!(
        engine.create_folder(new(Some(FOLDER), a.root().join("four"), Vec::new())),
        Err(Error::FolderExists(_))
    ));
    assert!(matches!(
        engine.create_folder(new(None, PathBuf::new(), Vec::new())),
        Err(Error::Invalid(_))
    ));
    engine.shutdown().await;
}
