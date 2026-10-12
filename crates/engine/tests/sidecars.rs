//! Sidecars (ADR 0034): every file the engine keeps in a folder's reserved directory has a JSON
//! file next to it saying what it is, so a rebuilt folder gets its recycle bin and history back.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode, Retention};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([35; 32]);

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
                // Scans are triggered explicitly so the tests are deterministic.
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

    /// The names of the files at the top of the folder, as they are on disk.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.root)
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
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

/// The sidecars under `.tidefell/<dir>`, with what they say.
fn sidecars(replica: &Replica, dir: &str) -> Vec<serde_json::Value> {
    let dir = replica.root.join(".tidefell").join(dir);
    let mut found: Vec<serde_json::Value> = fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                .map(|e| serde_json::from_slice(&fs::read(e.path()).unwrap()).unwrap())
                .collect()
        })
        .unwrap_or_default();
    found.sort_by_key(|v| v.to_string());
    found
}

#[tokio::test(flavor = "multi_thread")]
async fn kept_files_say_what_they_are_and_are_backfilled() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    a.write("gone.txt", b"deleted on a");
    a.write("edited.txt", b"first");
    a.write("here.txt", b"removed from b only");
    let engine_a = a.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    let engine_b = b.start(&user).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("the files reach b", || b.names().len() == 3).await;

    // Deleted elsewhere: b keeps the file for its recycle bin, and says which item it is.
    fs::remove_file(a.root.join("gone.txt")).unwrap();
    // Edited elsewhere: b keeps the earlier version in its history.
    a.write("edited.txt", b"second, longer");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("the changes reach b", || {
        b.read("gone.txt").is_none()
            && b.read("edited.txt").as_deref() == Some(b"second, longer".as_slice())
    })
    .await;
    // Removed from b only.
    engine_b.remove_here(FOLDER, "here.txt").await.unwrap();

    let kept = sidecars(&b, "deleted");
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0]["recycled"][0]["path"], "gone.txt");
    let history = sidecars(&b, "history");
    assert_eq!(history.len(), 1, "{history:?}");
    assert_eq!(history[0]["previous"]["path"], "edited.txt");
    let aside = sidecars(&b, "recycle");
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert_eq!(aside[0]["path"], "here.txt");

    // Lost, as files kept by an earlier version never had them: written again on start.
    engine_b.shutdown().await;
    for sub in ["deleted", "history", "recycle"] {
        for entry in fs::read_dir(b.root.join(".tidefell").join(sub))
            .unwrap()
            .flatten()
        {
            if entry.file_name().to_string_lossy().ends_with(".json") {
                fs::remove_file(entry.path()).unwrap();
            }
        }
    }
    let engine_b = b.start(&user).await;
    assert_eq!(sidecars(&b, "deleted"), kept);
    assert_eq!(sidecars(&b, "history"), history);
    assert_eq!(sidecars(&b, "recycle"), aside);

    // Restored, the set-aside file's sidecar goes with it.
    let entry = engine_b
        .recycle_bin(FOLDER)
        .unwrap()
        .into_iter()
        .find(|r| r.path == "here.txt")
        .unwrap()
        .entry;
    engine_b.restore(FOLDER, entry).await.unwrap();
    assert!(sidecars(&b, "recycle").is_empty());
    engine_b.shutdown().await;
}
