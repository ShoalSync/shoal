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
use tidefell_proto::{DeviceId, FolderId, UserId};

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
        self.launch(user, &[user.id()], network, peers, sync).await
    }

    /// Starts with `members` as the folder's members.
    async fn start_with_members(
        &self,
        user: &UserKey,
        members: &[UserId],
        network: &Network,
        peers: &[DeviceId],
    ) -> Engine {
        self.launch(user, members, network, peers, SyncOptions::default())
            .await
    }

    async fn launch(
        &self,
        user: &UserKey,
        members: &[UserId],
        network: &Network,
        peers: &[DeviceId],
        sync: SyncOptions,
    ) -> Engine {
        self.launch_as(user, members, network, peers, sync, ReplicaMode::Full)
            .await
    }

    /// A partial replica that keeps nothing it did not make: other devices' files stay
    /// placeholders until opened.
    async fn start_partial(
        &self,
        user: &UserKey,
        members: &[UserId],
        network: &Network,
        peers: &[DeviceId],
    ) -> Engine {
        let mode = ReplicaMode::Partial { budget_bytes: 0 };
        self.launch_as(user, members, network, peers, SyncOptions::default(), mode)
            .await
    }

    async fn launch_as(
        &self,
        user: &UserKey,
        members: &[UserId],
        network: &Network,
        peers: &[DeviceId],
        sync: SyncOptions,
        mode: ReplicaMode,
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
                mode,
                members: members.to_vec(),
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync,
                confirm_deletes: Default::default(),
                encryption: None,
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
        engine_a.revoke_device(&user, a.key.id(), None).is_err(),
        "not itself"
    );
    engine_a.revoke_device(&user, phone.key.id(), None).unwrap();
    assert_eq!(engine_a.revocations().len(), 1);

    // B was offline too: it hears of the revocation from A.
    let engine_b = b.start(&user, &network, &[a.key.id()]).await;
    eventually("b knows of the revocation", || {
        !engine_b.revocations().is_empty()
    })
    .await;
    eventually("b has a's files", || b.read("before.txt").is_some()).await;
    // The phone's file reached B as A's: A took it over when it revoked the phone (ADR 0040).
    let author = |engine: &Engine| {
        engine
            .files(FOLDER)
            .unwrap()
            .into_iter()
            .find(|f| f.path == "before.txt")
            .map(|f| f.author)
    };
    assert_eq!(author(&engine_b), Some(a.key.id()));
    assert_eq!(author(&engine_a), Some(a.key.id()));

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
    assert!(engine_b.revoke_device(&other, a.key.id(), None).is_ok());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(engine_b
        .connected_peers()
        .iter()
        .any(|p| p.device == a.key.id()));

    engine_phone.shutdown().await;
    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn versions_by_an_author_not_vouched_for_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let (alice, bob) = (UserKey::from_seed([1; 32]), UserKey::from_seed([2; 32]));
    let a = Replica::new(dir.path(), "a", 70);
    let b = Replica::new(dir.path(), "b", 71);
    let bobs = Replica::new(dir.path(), "bobs", 72);
    // A shares the folder with bob; B, alice's other device, does not.
    let engine_a = a
        .start_with_members(&alice, &[alice.id(), bob.id()], &network, &[])
        .await;
    let engine_bobs = bobs
        .start_with_members(&bob, &[alice.id(), bob.id()], &network, &[a.key.id()])
        .await;
    a.write("alice.txt", b"by alice");
    engine_a.scan(FOLDER).await.unwrap();
    bobs.write("bob.txt", b"by bob");
    engine_bobs.scan(FOLDER).await.unwrap();
    eventually("a has bob's file", || a.read("bob.txt").is_some()).await;

    // B hears of bob's version from A, can learn bob's device's certificate from A, and still
    // refuses it: bob is not a member of the folder on B.
    let engine_b = b.start(&alice, &network, &[a.key.id()]).await;
    eventually("b has alice's file", || b.read("alice.txt").is_some()).await;
    eventually("b has learnt the author's certificate", || {
        engine_b
            .certificates()
            .iter()
            .any(|c| c.verify_certificate().unwrap().device == bobs.key.id())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(b.read("bob.txt").is_none(), "refused");
    assert!(engine_b
        .files(FOLDER)
        .unwrap()
        .iter()
        .all(|f| f.path != "bob.txt"));

    // Bob removed on A: A takes bob's file over before refusing his devices' versions, so it
    // stays, now A's, and B gets it from A (ADR 0040, ADR 0024).
    engine_bobs.shutdown().await;
    engine_a.shutdown().await;
    let engine_a = a.start(&alice, &network, &[b.key.id()]).await;
    let author = engine_a
        .files(FOLDER)
        .unwrap()
        .into_iter()
        .find(|f| f.path == "bob.txt")
        .map(|f| f.author);
    assert_eq!(author, Some(a.key.id()), "taken over at start");
    eventually("b has bob's file as a's", || b.read("bob.txt").is_some()).await;

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_a_member_on_a_replica_without_their_files_deletes_nothing() {
    // Member lists are each replica's own: removing bob on alice's phone, a partial replica
    // without bob's file's content, must not delete it on the NAS, where bob is still a member.
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let (alice, bob) = (UserKey::from_seed([1; 32]), UserKey::from_seed([2; 32]));
    let nas = Replica::new(dir.path(), "nas", 80);
    let phone = Replica::new(dir.path(), "phone", 81);
    let bobs = Replica::new(dir.path(), "bobs", 82);
    let both = [alice.id(), bob.id()];
    let engine_nas = nas.start_with_members(&alice, &both, &network, &[]).await;
    let engine_bobs = bobs
        .start_with_members(&bob, &both, &network, &[nas.key.id()])
        .await;
    bobs.write("bob.txt", b"by bob");
    engine_bobs.scan(FOLDER).await.unwrap();
    eventually("the nas has bob's file", || nas.read("bob.txt").is_some()).await;
    engine_bobs.shutdown().await;

    let engine_phone = phone
        .start_partial(&alice, &both, &network, &[nas.key.id()])
        .await;
    eventually("the phone lists bob's file", || {
        engine_phone
            .files(FOLDER)
            .unwrap()
            .iter()
            .any(|f| f.path == "bob.txt")
    })
    .await;
    assert!(phone.read("bob.txt").is_none(), "a placeholder only");
    engine_phone.shutdown().await;

    // Bob removed on the phone alone.
    let engine_phone = phone
        .start_partial(&alice, &[alice.id()], &network, &[nas.key.id()])
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(nas.read("bob.txt").is_some(), "still on the nas");
    assert!(engine_nas
        .files(FOLDER)
        .unwrap()
        .iter()
        .any(|f| f.path == "bob.txt"));

    engine_phone.shutdown().await;
    engine_nas.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_time_of_loss_withdraws_what_came_after_it() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 90);
    let b = Replica::new(dir.path(), "b", 91);
    let phone = Replica::new(dir.path(), "phone", 92);
    let engine_a = a.start(&user, &network, &[]).await;
    let engine_b = b.start(&user, &network, &[a.key.id()]).await;
    let engine_phone = phone
        .start(&user, &network, &[a.key.id(), b.key.id()])
        .await;
    // Before the loss.
    phone.write("kept.txt", b"made before");
    phone.write("edited.txt", b"first, before");
    engine_phone.scan(FOLDER).await.unwrap();
    for replica in [&a, &b] {
        eventually("the phone's files arrive", || {
            replica.read("kept.txt").is_some()
                && replica.read("edited.txt").as_deref() == Some(b"first, before".as_slice())
        })
        .await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let lost = engine_a.now_ms();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // After it: the thief edits one file and makes another.
    phone.write("edited.txt", b"changed by the thief");
    phone.write("made.txt", b"made by the thief");
    engine_phone.scan(FOLDER).await.unwrap();
    for replica in [&a, &b] {
        eventually("the thief's changes arrive", || {
            replica.read("made.txt").is_some()
                && replica.read("edited.txt").as_deref() == Some(b"changed by the thief".as_slice())
        })
        .await;
    }
    engine_phone.shutdown().await;

    // A shows what it will do before it does it.
    let preview = engine_a.revoke_preview(phone.key.id(), lost);
    let mut listed: Vec<(String, bool)> = preview
        .iter()
        .flat_map(|p| p.withdrawn.iter().map(|f| (f.path.clone(), f.put_back)))
        .collect();
    listed.sort();
    assert_eq!(
        listed,
        vec![("edited.txt".into(), true), ("made.txt".into(), false)]
    );
    engine_a
        .revoke_device(&user, phone.key.id(), Some(lost))
        .unwrap();
    assert!(
        matches!(
            engine_a.revoke_device(&user, phone.key.id(), None),
            Err(tidefell_engine::Error::AlreadyRevoked)
        ),
        "once"
    );
    for (replica, engine) in [(&a, &engine_a), (&b, &engine_b)] {
        eventually("what came after the loss is withdrawn", || {
            replica.read("made.txt").is_none()
                && replica.read("edited.txt").as_deref() == Some(b"first, before".as_slice())
                && replica.read("kept.txt").as_deref() == Some(b"made before".as_slice())
        })
        .await;
        let authors: Vec<_> = engine
            .files(FOLDER)
            .unwrap()
            .into_iter()
            .map(|f| (f.path, f.author))
            .collect();
        assert!(
            authors.iter().all(|(_, author)| *author != phone.key.id()),
            "{authors:?}"
        );
        assert!(
            engine
                .recycle_bin(FOLDER)
                .unwrap()
                .iter()
                .any(|r| r.path == "made.txt"),
            "in the recycle bin"
        );
    }

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_time_of_loss_puts_back_a_valid_change_made_after_it() {
    // The loss came before another device's edit, which the thief then changed: what goes back
    // is that edit, not what was there before the loss.
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 93);
    let b = Replica::new(dir.path(), "b", 94);
    let phone = Replica::new(dir.path(), "phone", 95);
    let engine_a = a.start(&user, &network, &[]).await;
    let engine_b = b.start(&user, &network, &[a.key.id()]).await;
    let engine_phone = phone
        .start(&user, &network, &[a.key.id(), b.key.id()])
        .await;
    a.write("notes.txt", b"before");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("everyone has it", || {
        phone.read("notes.txt").is_some() && b.read("notes.txt").is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let lost = engine_a.now_ms();
    tokio::time::sleep(Duration::from_millis(100)).await;
    b.write("notes.txt", b"b's edit after the loss");
    engine_b.scan(FOLDER).await.unwrap();
    eventually("the phone has b's edit", || {
        phone.read("notes.txt").as_deref() == Some(b"b's edit after the loss".as_slice())
            && a.read("notes.txt").as_deref() == Some(b"b's edit after the loss".as_slice())
    })
    .await;
    phone.write("notes.txt", b"the thief's");
    engine_phone.scan(FOLDER).await.unwrap();
    eventually("the thief's edit arrives", || {
        a.read("notes.txt").as_deref() == Some(b"the thief's".as_slice())
            && b.read("notes.txt").as_deref() == Some(b"the thief's".as_slice())
    })
    .await;
    engine_phone.shutdown().await;

    engine_a
        .revoke_device(&user, phone.key.id(), Some(lost))
        .unwrap();
    for replica in [&a, &b] {
        eventually("b's edit is put back", || {
            replica.read("notes.txt").as_deref() == Some(b"b's edit after the loss".as_slice())
        })
        .await;
    }
    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn many_withdrawals_wait_for_the_user() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 96);
    let b = Replica::new(dir.path(), "b", 97);
    let phone = Replica::new(dir.path(), "phone", 98);
    let engine_a = a.start(&user, &network, &[]).await;
    let engine_b = b.start(&user, &network, &[a.key.id()]).await;
    let engine_phone = phone.start(&user, &network, &[a.key.id()]).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let lost = engine_a.now_ms();
    tokio::time::sleep(Duration::from_millis(100)).await;
    for i in 0..120 {
        phone.write(&format!("thief/{i}.txt"), format!("{i}").as_bytes());
    }
    engine_phone.scan(FOLDER).await.unwrap();
    eventually("the thief's files arrive", || {
        a.read("thief/119.txt").is_some() && b.read("thief/119.txt").is_some()
    })
    .await;
    engine_phone.shutdown().await;

    engine_a
        .revoke_device(&user, phone.key.id(), Some(lost))
        .unwrap();
    // Every one of the folder's files would go: each device asks first (ADR 0040, ADR 0037).
    eventually("a holds the withdrawals", || {
        engine_a
            .held_withdrawals(FOLDER)
            .first()
            .is_some_and(|(device, paths)| *device == phone.key.id() && paths.len() == 120)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(a.read("thief/0.txt").is_some(), "nothing withdrawn unasked");

    engine_a
        .decide_withdrawals(FOLDER, phone.key.id(), true)
        .unwrap();
    eventually("a withdraws them once told to", || {
        a.read("thief/0.txt").is_none()
    })
    .await;
    assert!(engine_a.held_withdrawals(FOLDER).is_empty());
    // B asks too, and is told separately.
    eventually("b holds them too", || {
        !engine_b.held_withdrawals(FOLDER).is_empty()
    })
    .await;
    engine_b
        .decide_withdrawals(FOLDER, phone.key.id(), true)
        .unwrap();
    eventually("b withdraws them once told to", || {
        b.read("thief/0.txt").is_none()
    })
    .await;

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn another_users_revocation_of_a_device_neither_revokes_it_nor_blocks_its_own() {
    // Bob shares the folder, and signs a revocation of alice's phone's device ID: it revokes
    // nothing, and alice can still revoke her phone (ADR 0040).
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let (alice, bob) = (UserKey::from_seed([1; 32]), UserKey::from_seed([2; 32]));
    let both = [alice.id(), bob.id()];
    let a = Replica::new(dir.path(), "a", 100);
    let bobs = Replica::new(dir.path(), "bobs", 101);
    let phone = Replica::new(dir.path(), "phone", 102);
    let engine_a = a.start_with_members(&alice, &both, &network, &[]).await;
    let engine_bobs = bobs
        .start_with_members(&bob, &both, &network, &[a.key.id()])
        .await;
    let engine_phone = phone
        .start_with_members(&alice, &both, &network, &[a.key.id()])
        .await;
    eventually("all connected", || engine_a.connected_peers().len() == 2).await;

    engine_bobs
        .revoke_device(&bob, phone.key.id(), None)
        .unwrap();
    eventually("a hears of bob's revocation", || {
        engine_a.revocations().len() == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        engine_a
            .connected_peers()
            .iter()
            .any(|p| p.device == phone.key.id()),
        "not revoked by bob's"
    );

    engine_a
        .revoke_device(&alice, phone.key.id(), None)
        .unwrap();
    eventually("alice's revocation takes", || {
        engine_a
            .connected_peers()
            .iter()
            .all(|p| p.device != phone.key.id())
    })
    .await;
    engine_phone.shutdown().await;
    engine_bobs.shutdown().await;
    engine_a.shutdown().await;
}
