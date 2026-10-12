//! Version history (ADR 0029): content the engine replaces is kept on this device as previous
//! versions, and restoring one makes it the file's content again everywhere.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode, Retention, VersionKind,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([32; 32]);

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

#[tokio::test(flavor = "multi_thread")]
async fn a_replaced_file_is_restored_from_its_history() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([7; 32]);
    let a = Replica::new(dir.path(), "a", 70);
    let b = Replica::new(dir.path(), "b", 71);
    a.write("notes.txt", b"first draft");
    let engine_a = a.start(&user).await;
    let engine_b = b.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("the first draft reaches b", || {
        b.read("notes.txt").as_deref() == Some(b"first draft".as_slice())
    })
    .await;

    // A's edit replaces b's copy: b keeps the first draft.
    a.write("notes.txt", b"second draft, worse");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("the edit reaches b", || {
        b.read("notes.txt").as_deref() == Some(b"second draft, worse".as_slice())
    })
    .await;
    let versions = engine_b.versions(FOLDER, "notes.txt").unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].kind, VersionKind::Current);
    assert_eq!(versions[0].author, Some(a.key.id()));
    assert_eq!(versions[1].kind, VersionKind::Previous);
    assert_eq!(versions[1].size, 11);
    let first = engine_b
        .version_file(FOLDER, "notes.txt", &versions[1].id)
        .unwrap();
    assert_eq!(fs::read(first).unwrap(), b"first draft");
    // History stays out of the phone gallery on Android.
    assert!(b.root.join(".tidefell/.nomedia").exists());
    // A's edit was made on disk, so a keeps nothing (ADR 0029).
    assert_eq!(engine_a.versions(FOLDER, "notes.txt").unwrap().len(), 1);

    // Restoring on b makes the first draft current on both devices.
    engine_b
        .restore_version(FOLDER, "notes.txt", &versions[1].id)
        .await
        .unwrap();
    assert_eq!(
        b.read("notes.txt").as_deref(),
        Some(b"first draft".as_slice())
    );
    eventually("the restore reaches a", || {
        a.read("notes.txt").as_deref() == Some(b"first draft".as_slice())
    })
    .await;
    // The second draft is now b's previous version, and a kept it when it was replaced there.
    let on_b = engine_b.versions(FOLDER, "notes.txt").unwrap();
    assert_eq!(on_b[0].author, Some(b.key.id()));
    let kept: Vec<Vec<u8>> = on_b[1..]
        .iter()
        .map(|v| fs::read(engine_b.version_file(FOLDER, "notes.txt", &v.id).unwrap()).unwrap())
        .collect();
    assert!(kept.contains(&b"second draft, worse".to_vec()));
    eventually("a keeps the second draft", || {
        engine_a.versions(FOLDER, "notes.txt").unwrap().len() == 2
    })
    .await;
    let recycled = engine_b.folders()[0].recycle_bin.bytes;
    assert!(
        recycled >= 11 + 19,
        "history counts toward the recycle bin: {recycled}"
    );
}
