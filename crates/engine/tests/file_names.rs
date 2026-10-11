//! File names across filesystems (ADR 0033): two engines with real iroh endpoints on localhost
//! and real folders on disk.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, ProblemKind, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([33; 32]);

/// "résumé" with "é" as one code point (NFC) and as "e" with a combining accent (NFD).
const NFC: &str = "r\u{e9}sum\u{e9}.txt";
const NFD: &str = "re\u{301}sume\u{301}.txt";

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

fn paths(engine: &Engine) -> Vec<String> {
    engine
        .files(FOLDER)
        .unwrap()
        .into_iter()
        .map(|f| f.path)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn names_are_recorded_in_nfc_and_keep_their_spelling_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    // A filesystem that keeps names as given, as Linux does, holding a name in NFD.
    a.write(NFD, b"coffee");

    let engine_a = a.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(paths(&engine_a), [NFC]);
    let file = engine_a.files(FOLDER).unwrap().remove(0);
    assert!(file.on_disk, "the file on disk is the recorded one");
    // Scanned again, nothing changed.
    let status = engine_a.status(FOLDER).unwrap();
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(engine_a.status(FOLDER).unwrap().files, status.files);
    assert_eq!(a.names(), [NFD], "the name keeps its spelling on disk");

    // Another device writes the name as recorded.
    let engine_b = b.start(&user).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("the file reaches b", || {
        b.read(NFC).as_deref() == Some(b"coffee".as_slice())
    })
    .await;
    assert_eq!(b.names(), [NFC]);

    // An edit on b is written to a under a's spelling, and is not a second file.
    b.write(NFC, b"tea");
    engine_b.scan(FOLDER).await.unwrap();
    eventually("the edit reaches a", || {
        a.read(NFD).as_deref() == Some(b"tea".as_slice())
    })
    .await;
    assert_eq!(a.names(), [NFD]);
    assert_eq!(paths(&engine_a), [NFC]);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_name_written_two_ways_is_one_file_and_a_problem() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    a.write(NFD, b"first");
    let engine_a = a.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();

    // The same name in NFC beside it, which Linux allows.
    a.write(NFC, b"second");
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(paths(&engine_a), [NFC]);
    assert_eq!(
        engine_a.files(FOLDER).unwrap()[0].size,
        5,
        "the file it had keeps the path"
    );
    let problems = engine_a.path_problems(FOLDER).unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].path, NFC);
    assert_eq!(problems[0].kind, ProblemKind::NameNotAllowed);
}
