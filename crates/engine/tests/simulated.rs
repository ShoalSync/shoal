//! Engines on the simulated network of ADR 0036, as `replicas.rs` runs them on iroh: the same
//! syncing, plus a partition the engines reconnect after on their own.
#![cfg(feature = "simulation")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Direction, Engine, EngineConfig, FolderConfig, FolderStatus, PeerAddr, RelayConfig,
    ReplicaMode, Retention, SyncOptions,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_net::Network;
use tidefell_proto::{DeviceId, FolderId};

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

    async fn start(&self, user: &UserKey, network: &Network, peers: &[DeviceId]) -> Engine {
        self.start_with(user, network, peers, SyncOptions::default())
            .await
    }

    async fn start_with(
        &self,
        user: &UserKey,
        network: &Network,
        peers: &[DeviceId],
        sync: SyncOptions,
    ) -> Engine {
        let config = EngineConfig {
            data_dir: self.data.clone(),
            metadata_dir: None,
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            limits: Default::default(),
            bind: None,
            peers: peers
                .iter()
                .map(|device| PeerAddr {
                    device: *device,
                    addrs: Vec::new(),
                    relay: None,
                })
                .collect(),
            folders: vec![FolderConfig {
                id: FOLDER,
                path: self.root.clone(),
                mode: ReplicaMode::Full,
                members: vec![user.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync,
                confirm_deletes: Default::default(),
            }],
            min_free_space: None,
        };
        let certificate = user.certify(self.key.id(), "test", 0);
        let engine = Engine::start_simulated(config, self.key.clone(), certificate, network)
            .await
            .unwrap();
        engine.set_retry_interval(Duration::from_millis(50));
        engine
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

#[tokio::test(flavor = "multi_thread")]
async fn engines_sync_on_the_simulated_network() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    let big: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    a.write("hello.txt", b"hello from a");
    a.write("nested/dir/big.bin", &big);

    let engine_a = a.start(&user, &network, &[]).await;
    let engine_b = b.start(&user, &network, &[]).await;
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("initial files on b", || {
        b.read("hello.txt").as_deref() == Some(b"hello from a".as_slice())
            && b.read("nested/dir/big.bin").as_deref() == Some(big.as_slice())
    })
    .await;

    b.write("hello.txt", b"edited on b");
    engine_b.scan(FOLDER).await.unwrap();
    eventually("edit reaches a", || {
        a.read("hello.txt").as_deref() == Some(b"edited on b".as_slice())
    })
    .await;

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

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn engines_reconnect_after_a_partition_heals() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 20);
    let b = Replica::new(dir.path(), "b", 21);
    a.write("doc.txt", b"original");
    let (id_a, id_b) = (a.key.id(), b.key.id());
    let engine_a = a.start(&user, &network, &[id_b]).await;
    let engine_b = b.start(&user, &network, &[id_a]).await;
    engine_a.scan(FOLDER).await.unwrap();
    eventually("b connects on its own", || b.read("doc.txt").is_some()).await;

    network.partition([(id_b, 1)]);
    a.write("doc.txt", b"edited while apart");
    engine_a.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(b.read("doc.txt").as_deref(), Some(b"original".as_slice()));

    network.heal();
    eventually("the edit arrives once healed", || {
        b.read("doc.txt").as_deref() == Some(b"edited while apart".as_slice())
    })
    .await;

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn content_a_read_missed_is_offered_again_after_a_scan() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 30);
    let b = Replica::new(dir.path(), "b", 31);
    let c = Replica::new(dir.path(), "c", 32);
    a.write("doc.txt", b"only on a");
    // Send-only, so not a full replica: b and c, with a choice of devices, ask a which chunks
    // it has instead of asking for them (ADR 0032).
    let send_only = SyncOptions {
        direction: Direction::SendOnly,
        ..Default::default()
    };
    let engine_a = a.start_with(&user, &network, &[], send_only).await;
    engine_a.scan(FOLDER).await.unwrap();
    // Unreadable while it is asked for, as a file is while it is moved: a's read fails, and a
    // stops counting the content as here.
    let file = a.root.join("doc.txt");
    let mode = |m| fs::set_permissions(&file, fs::Permissions::from_mode(m)).unwrap();
    mode(0o000);
    if fs::read(&file).is_ok() {
        // Running as root, which reads it anyway.
        engine_a.shutdown().await;
        return;
    }
    let engine_b = b.start(&user, &network, &[]).await;
    let engine_c = c.start(&user, &network, &[]).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    engine_b.connect(engine_c.addr()).await.unwrap();
    engine_c.connect(engine_a.addr()).await.unwrap();
    eventually("b and c find it unavailable", || {
        [&engine_b, &engine_c].iter().all(|e| {
            e.path_problems(FOLDER)
                .unwrap()
                .iter()
                .any(|p| p.path == "doc.txt")
        })
    })
    .await;

    // The file is readable again, unchanged: once a scans, a offers it again. It is the only
    // copy, so it would otherwise be unavailable for good.
    mode(0o644);
    engine_a.scan(FOLDER).await.unwrap();
    eventually("b and c get it", || {
        b.read("doc.txt").as_deref() == Some(b"only on a".as_slice())
            && c.read("doc.txt").as_deref() == Some(b"only on a".as_slice())
    })
    .await;

    engine_b.shutdown().await;
    engine_c.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn engines_keep_the_certificates_of_devices_they_meet() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 50);
    let b = Replica::new(dir.path(), "b", 51);
    let engine_a = a.start(&user, &network, &[]).await;
    let engine_b = b.start(&user, &network, &[]).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    let knows = |engine: &Engine, device: DeviceId| {
        engine
            .certificates()
            .iter()
            .any(|c| c.verify_certificate().unwrap().device == device)
    };
    eventually(
        "each knows the other's certificate from the handshake",
        || knows(&engine_a, b.key.id()) && knows(&engine_b, a.key.id()),
    )
    .await;
    engine_b.shutdown().await;
    engine_a.shutdown().await;

    // Kept in the runtime settings: still known after a restart, without meeting again.
    let engine_a = a.start(&user, &network, &[]).await;
    assert!(knows(&engine_a, b.key.id()), "kept across a restart");
    assert!(knows(&engine_a, a.key.id()), "its own too");
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_device_is_refused_by_every_peer_that_hears_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 60);
    let b = Replica::new(dir.path(), "b", 61);
    let phone = Replica::new(dir.path(), "phone", 62);
    phone.write("before.txt", b"before the loss");
    let engine_a = a.start(&user, &network, &[]).await;
    let engine_phone = phone.start(&user, &network, &[a.key.id()]).await;
    engine_phone.scan(FOLDER).await.unwrap();
    eventually("a has the phone's file", || a.read("before.txt").is_some()).await;
    // The phone is lost, offline: it does not hear of what follows.
    engine_phone.shutdown().await;

    assert!(
        engine_a.revoke_device(&user, a.key.id()).is_err(),
        "not itself"
    );
    engine_a.revoke_device(&user, phone.key.id()).unwrap();
    assert_eq!(engine_a.revocations().len(), 1);

    // B was offline too: it hears of the revocation from A.
    let engine_b = b.start(&user, &network, &[a.key.id()]).await;
    eventually("b knows of the revocation", || {
        !engine_b.revocations().is_empty()
    })
    .await;
    eventually("b has a's files", || b.read("before.txt").is_some()).await;

    // The phone comes back, and reaches B first: B refuses it, and tells it why.
    phone.write("after.txt", b"by the thief");
    let engine_phone = phone.start(&user, &network, &[b.key.id()]).await;
    engine_phone.scan(FOLDER).await.unwrap();
    eventually("the phone learns it is revoked", || engine_phone.revoked()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(b.read("after.txt").is_none(), "nothing from the phone");
    assert!(engine_b
        .connected_peers()
        .iter()
        .all(|p| p.device != phone.key.id()));
    // Kept across a restart.
    engine_b.shutdown().await;
    let engine_b = b.start(&user, &network, &[a.key.id()]).await;
    assert_eq!(engine_b.revocations().len(), 1);

    // Another user's revocation of the same device ID revokes nothing.
    let other = UserKey::from_seed([9; 32]);
    assert!(engine_b.revoke_device(&other, a.key.id()).is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(engine_b
        .connected_peers()
        .iter()
        .any(|p| p.device == a.key.id()));

    engine_phone.shutdown().await;
    engine_b.shutdown().await;
    engine_a.shutdown().await;
}
