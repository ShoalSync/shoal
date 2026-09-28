//! Status and problems (ADR 0030): peers connected or not, folder states, transfer totals and
//! paths that fail.

use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, FolderState, Operation, PathKind, PeerAddr, ProblemKind,
    RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([43; 32]);

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

    async fn start(&self, user: &UserKey, peers: Vec<PeerAddr>) -> Engine {
        let config = EngineConfig {
            data_dir: self.data.clone(),
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            limits: Default::default(),
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            peers,
            folders: vec![FolderConfig {
                id: FOLDER,
                path: self.root.clone(),
                mode: ReplicaMode::Full,
                members: vec![user.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
            }],
        };
        let certificate = user.certify(self.key.id(), "laptop", 0);
        Engine::start(config, self.key.clone(), certificate)
            .await
            .unwrap()
    }

    fn write(&self, path: &str, data: &[u8]) {
        let abs = self.root.join(path);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, data).unwrap();
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

fn chmod(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Permissions do not stop root, so the permission tests only mean something for other users.
fn is_root(dir: &Path) -> bool {
    let probe = dir.join("probe");
    fs::write(&probe, b"x").unwrap();
    chmod(&probe, 0o000);
    let root = fs::read(&probe).is_ok();
    chmod(&probe, 0o644);
    fs::remove_file(probe).unwrap();
    root
}

#[tokio::test(flavor = "multi_thread")]
async fn peers_folders_and_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 50);
    let b = Replica::new(dir.path(), "b", 51);
    let content = vec![7u8; 300_000];
    a.write("big.bin", &content);

    // b keeps a device connected that is never reachable.
    let nowhere = DeviceKey::from_seed([52; 32]).id();
    let unreachable = PeerAddr {
        device: nowhere,
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 9))],
        relay: None,
    };
    let engine_a = a.start(&user, Vec::new()).await;
    let engine_b = b.start(&user, vec![unreachable]).await;
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("b has the file", || {
        fs::read(b.root.join("big.bin")).is_ok_and(|d| d == content)
    })
    .await;

    // a is connected directly, and the unreachable device is listed with its error.
    eventually("the dialer tried the unreachable device", || {
        engine_b
            .peer_states()
            .iter()
            .any(|p| p.device == nowhere && p.last_error.is_some())
    })
    .await;
    let peers = engine_b.peer_states();
    let to_a = peers.iter().find(|p| p.device == engine_a.id()).unwrap();
    assert!(to_a.connected);
    assert_eq!(to_a.path, Some(PathKind::Direct));
    assert!(to_a.rtt_ms.is_some());
    assert_eq!(to_a.name.as_deref(), Some("laptop"));
    assert!(to_a.connected_since_ms.is_some() && to_a.last_error.is_none());
    let lost = peers.iter().find(|p| p.device == nowhere).unwrap();
    assert!(!lost.connected && lost.path.is_none() && lost.name.is_none());

    // The file's content went from a to b.
    assert!(engine_a.transfer().sent >= content.len() as u64);
    assert!(engine_b.transfer().received >= content.len() as u64);

    // b's folder is caught up with a.
    eventually("b's folder is idle and up to date with a", || {
        let folder = &engine_b.folders()[0];
        folder.state == FolderState::Idle
            && folder.pending_files == 0
            && folder
                .peers
                .iter()
                .any(|p| p.device == engine_a.id() && p.up_to_date && p.last_exchange_ms.is_some())
    })
    .await;
    engine_b.scan(FOLDER).await.unwrap();
    let folder = &engine_b.folders()[0];
    assert!(folder.last_scan_ms.is_some() && folder.last_scan_duration_ms.is_some());
    assert_eq!(folder.problems, 0);

    // Disconnected, a is still listed, with when it was last seen.
    let a_id = engine_a.id();
    engine_a.shutdown().await;
    eventually("a shows as disconnected", || {
        engine_b
            .peer_states()
            .iter()
            .any(|p| p.device == a_id && !p.connected && p.last_seen_ms.is_some())
    })
    .await;
    engine_b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unreadable_files_are_problems_until_they_read() {
    let dir = tempfile::tempdir().unwrap();
    if is_root(dir.path()) {
        return;
    }
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 53);
    a.write("secret.txt", b"locked");
    a.write("gone.txt", b"locked too");
    a.write("dir/inside.txt", b"in a locked directory");
    a.write("fine.txt", b"readable");
    let engine = a.start(&user, Vec::new()).await;
    engine.scan(FOLDER).await.unwrap();
    // Recorded before the directory is locked, so its file must not be taken for deleted.
    assert!(engine
        .files(FOLDER)
        .unwrap()
        .iter()
        .any(|f| f.path == "dir/inside.txt"));
    // Changed since, so the next scan reads them.
    tokio::time::sleep(Duration::from_millis(20)).await;
    a.write("secret.txt", b"locked, and changed");
    a.write("gone.txt", b"locked too, and changed");
    chmod(&a.root.join("secret.txt"), 0o000);
    chmod(&a.root.join("gone.txt"), 0o000);
    chmod(&a.root.join("dir"), 0o000);
    engine.scan(FOLDER).await.unwrap();

    let problems = engine.path_problems(FOLDER).unwrap();
    let find = |path: &str| problems.iter().find(|p| p.path == path).cloned();
    let secret = find("secret.txt").expect("the unreadable file is a problem");
    assert_eq!(secret.kind, ProblemKind::PermissionDenied);
    assert_eq!((secret.operation, secret.attempts), (Operation::Scan, 1));
    assert_eq!(find("dir/").unwrap().kind, ProblemKind::PermissionDenied);
    assert!(find("fine.txt").is_none());
    assert!(
        engine
            .files(FOLDER)
            .unwrap()
            .iter()
            .any(|f| f.path == "dir/inside.txt"),
        "a file in an unreadable directory is not deleted"
    );
    assert_eq!(engine.folders()[0].problems, 3);

    // Failing again counts; reading again, or the file going away, clears it.
    engine.scan(FOLDER).await.unwrap();
    let again = engine.path_problems(FOLDER).unwrap();
    let secret = again.iter().find(|p| p.path == "secret.txt").unwrap();
    assert_eq!(
        (secret.attempts, secret.since_ms),
        (2, find("secret.txt").unwrap().since_ms)
    );
    chmod(&a.root.join("secret.txt"), 0o644);
    chmod(&a.root.join("dir"), 0o755);
    fs::remove_file(a.root.join("gone.txt")).unwrap();
    engine.scan(FOLDER).await.unwrap();
    assert!(engine.path_problems(FOLDER).unwrap().is_empty());
    engine.shutdown().await;
}

/// A directory that can be listed but not entered: its files cannot be looked at, which must
/// not make them look deleted.
#[tokio::test(flavor = "multi_thread")]
async fn files_in_an_unsearchable_directory_are_not_deleted() {
    let dir = tempfile::tempdir().unwrap();
    if is_root(dir.path()) {
        return;
    }
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 56);
    a.write("dir/x.txt", b"keep me");
    let engine = a.start(&user, Vec::new()).await;
    engine.scan(FOLDER).await.unwrap();
    chmod(&a.root.join("dir"), 0o644);
    engine.scan(FOLDER).await.unwrap();
    let kept = engine
        .files(FOLDER)
        .unwrap()
        .iter()
        .any(|f| f.path == "dir/x.txt");
    let problems = engine.path_problems(FOLDER).unwrap();
    chmod(&a.root.join("dir"), 0o755);
    assert!(kept, "the file is not taken for deleted");
    assert!(
        problems
            .iter()
            .any(|p| p.path == "dir/x.txt" && p.kind == ProblemKind::PermissionDenied),
        "{problems:?}"
    );
    engine.scan(FOLDER).await.unwrap();
    assert!(engine.path_problems(FOLDER).unwrap().is_empty());
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_folder_lists_every_file_it_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    if is_root(dir.path()) {
        return;
    }
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 54);
    let b = Replica::new(dir.path(), "b", 55);
    let engine_a = a.start(&user, Vec::new()).await;
    let engine_b = b.start(&user, Vec::new()).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    // b's folder cannot be written to, as on a read-only disk.
    chmod(&b.root, 0o555);
    for name in ["one.txt", "two.txt", "three.txt"] {
        a.write(name, name.as_bytes());
    }
    engine_a.scan(FOLDER).await.unwrap();
    eventually("every file is a write problem on b", || {
        let problems = engine_b.path_problems(FOLDER).unwrap();
        problems.len() == 3
            && problems
                .iter()
                .all(|p| p.operation == Operation::Write && p.kind == ProblemKind::PermissionDenied)
    })
    .await;

    // Writable again: the files arrive and the problems clear.
    chmod(&b.root, 0o755);
    eventually("the files arrive", || {
        fs::read(b.root.join("three.txt")).is_ok()
            && engine_b.path_problems(FOLDER).unwrap().is_empty()
    })
    .await;
    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_doctor_checks_clocks_and_folders() {
    use tidefell_engine::CheckLevel;

    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 57);
    let b = Replica::new(dir.path(), "b", 58);
    let engine_a = a.start(&user, Vec::new()).await;
    let engine_b = b.start(&user, Vec::new()).await;
    engine_b.connect(engine_a.addr()).await.unwrap();

    let checks = engine_b.doctor();
    let find = |name: &str| checks.iter().filter(|c| c.name == name).collect::<Vec<_>>();
    // The peer's Hello carried its clock; on one machine the clocks agree.
    let clock = find("clock");
    assert_eq!(clock.len(), 1, "{checks:?}");
    assert_eq!(clock[0].level, CheckLevel::Pass);
    assert!(clock[0].detail.contains("laptop"));
    assert_eq!(find("folder")[0].level, CheckLevel::Pass);
    assert_eq!(find("state")[0].level, CheckLevel::Pass);
    // With relays turned off there is no network report to judge, so nothing warns.
    assert!(
        checks.iter().all(|c| c.level == CheckLevel::Pass),
        "{checks:?}"
    );

    // A folder this device cannot write to fails, with advice.
    if !is_root(dir.path()) {
        chmod(&b.root, 0o555);
        let checks = engine_b.doctor();
        chmod(&b.root, 0o755);
        let folder = checks.iter().find(|c| c.name == "folder").unwrap();
        assert_eq!(folder.level, CheckLevel::Failure, "{folder:?}");
        assert!(folder.advice.is_some());
    }
    engine_b.shutdown().await;
    engine_a.shutdown().await;
}
