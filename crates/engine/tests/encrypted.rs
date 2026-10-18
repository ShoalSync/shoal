//! Encrypted folders between member devices on the simulated network (ADR 0023): each makes or
//! is granted the folder's keys, and versions travel sealed over sync/2. Content follows over
//! blobs/2 in a later step; here the metadata syncs.
#![cfg(feature = "simulation")]

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Encryption, Engine, EngineConfig, FolderConfig, PeerAddr, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_net::Network;
use tidefell_proto::{DeviceId, FolderId, UserId};

const FOLDER: FolderId = FolderId([43; 32]);

struct Member {
    root: PathBuf,
    data: PathBuf,
    key: DeviceKey,
}

impl Member {
    fn new(dir: &Path, name: &str, seed: u8) -> Self {
        Self {
            root: dir.join(name).join("folder"),
            data: dir.join(name).join("data"),
            key: DeviceKey::from_seed([seed; 32]),
        }
    }

    async fn start(&self, user: &UserKey, network: &Network, peers: &[DeviceId]) -> Engine {
        self.start_with(user, &[user.id()], &[], network, peers)
            .await
    }

    /// With `members` as the folder's members and `storage_peers` as its storage peers.
    async fn start_with(
        &self,
        user: &UserKey,
        members: &[UserId],
        storage_peers: &[DeviceId],
        network: &Network,
        peers: &[DeviceId],
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
                members: members.to_vec(),
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
                confirm_deletes: Default::default(),
                encryption: Some(Encryption {
                    storage_peers: storage_peers.to_vec(),
                }),
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

fn paths(engine: &Engine) -> Vec<String> {
    let mut paths: Vec<String> = engine
        .files(FOLDER)
        .unwrap()
        .into_iter()
        .map(|f| f.path)
        .collect();
    paths.sort();
    paths
}

#[tokio::test(flavor = "multi_thread")]
async fn members_sync_an_encrypted_folders_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([3; 32]);
    let a = Member::new(dir.path(), "a", 30);
    let b = Member::new(dir.path(), "b", 31);
    a.write("notes/plan.txt", b"sealed on a");
    let (id_a, id_b) = (a.key.id(), b.key.id());
    let engine_a = a.start(&user, &network, &[id_b]).await;
    let engine_b = b.start(&user, &network, &[id_a]).await;
    engine_a.scan(FOLDER).await.unwrap();

    // b opens what a sealed once a grant reaches it, fetches its sealed chunks and writes it;
    // and a b's.
    eventually("b has a's file", || {
        b.read("notes/plan.txt").as_deref() == Some(b"sealed on a".as_slice())
    })
    .await;
    let big: Vec<u8> = (0..3 * 1024 * 1024u32)
        .map(|i| (i * 7 % 251) as u8)
        .collect();
    b.write("from-b.bin", &big);
    engine_b.scan(FOLDER).await.unwrap();
    eventually("a has b's file of several chunks", || {
        a.read("from-b.bin").as_deref() == Some(big.as_slice())
    })
    .await;
    assert_eq!(paths(&engine_a), vec!["from-b.bin", "notes/plan.txt"]);

    // An edit and a delete go the same way.
    a.write("notes/plan.txt", b"edited on a");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("the edit reaches b", || {
        b.read("notes/plan.txt").as_deref() == Some(b"edited on a".as_slice())
    })
    .await;
    fs::remove_file(b.root.join("from-b.bin")).unwrap();
    engine_b.scan(FOLDER).await.unwrap();
    eventually("the delete reaches a", || a.read("from-b.bin").is_none()).await;
    // Never over sync/1: a sync/1 session, if any, shares nothing of the encrypted folder.
    assert!(engine_a
        .connected_peers()
        .iter()
        .all(|p| !p.shared_folders.contains(&FOLDER)));

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_member_keeps_its_keys() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([4; 32]);
    let a = Member::new(dir.path(), "a", 40);
    let b = Member::new(dir.path(), "b", 41);
    let (id_a, id_b) = (a.key.id(), b.key.id());
    let engine_a = a.start(&user, &network, &[id_b]).await;
    let engine_b = b.start(&user, &network, &[id_a]).await;
    a.write("first.txt", b"one");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("b sees first.txt", || paths(&engine_b) == vec!["first.txt"]).await;

    // b restarts: its keys and what it opened are kept, and what a seals next it opens too.
    engine_b.shutdown().await;
    let engine_b = b.start(&user, &network, &[id_a]).await;
    assert_eq!(paths(&engine_b), vec!["first.txt"]);
    a.write("second.txt", b"two");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("b sees second.txt", || {
        paths(&engine_b) == vec!["first.txt", "second.txt"]
    })
    .await;

    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

/// A device whose user is not a member is never granted a key, even when it is offered the
/// folder and receives its sealed versions, as a storage peer is (ADR 0023): it reads nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_of_a_user_not_a_member_gets_no_key() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([5; 32]);
    let stranger = UserKey::from_seed([6; 32]);
    let a = Member::new(dir.path(), "a", 50);
    let c = Member::new(dir.path(), "c", 52);
    let (id_a, id_c) = (a.key.id(), c.key.id());
    a.write("secret.txt", b"members only");
    // a lists c as a storage peer, so a offers c the folder; c counts a's user as a member, so
    // c takes what a sends and publishes its own encryption key. a vouches for no device of
    // c's user, so it grants c nothing.
    let engine_a = a
        .start_with(&user, &[user.id()], &[id_c], &network, &[id_c])
        .await;
    let engine_c = c
        .start_with(
            &stranger,
            &[user.id(), stranger.id()],
            &[],
            &network,
            &[id_a],
        )
        .await;
    engine_a.scan(FOLDER).await.unwrap();
    // c gets a's sealed versions but cannot open them.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(paths(&engine_c).is_empty());
    // And a has not taken c's versions either: c is not vouched for by a's member list.
    c.write("from-c.txt", b"not a member's");
    engine_c.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(paths(&engine_a), vec!["secret.txt"]);

    engine_c.shutdown().await;
    engine_a.shutdown().await;
}

fn generation(engine: &Engine) -> Option<u64> {
    engine.folder_key_generation(FOLDER).unwrap()
}

/// Revoking a device that holds the folder key rotates it (ADR 0023): the devices that remain
/// make and share the next generation and seal what comes after with it; the revoked device
/// never gets it.
#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_device_rotates_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([7; 32]);
    let a = Member::new(dir.path(), "a", 70);
    let b = Member::new(dir.path(), "b", 71);
    let c = Member::new(dir.path(), "c", 72);
    let (id_a, id_b, id_c) = (a.key.id(), b.key.id(), c.key.id());
    let engine_a = a.start(&user, &network, &[id_b, id_c]).await;
    let engine_b = b.start(&user, &network, &[id_a, id_c]).await;
    let engine_c = c.start(&user, &network, &[id_a, id_b]).await;
    a.write("before.txt", b"everyone reads this");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("c has a's file", || {
        c.read("before.txt").as_deref() == Some(b"everyone reads this".as_slice())
    })
    .await;
    assert_eq!(generation(&engine_a), Some(1));

    engine_a.revoke_device(&user, id_c, None).unwrap();
    eventually("a and b hold the next generation", || {
        generation(&engine_a) == Some(2) && generation(&engine_b) == Some(2)
    })
    .await;
    a.write("after.txt", b"not for c");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("b has the file written after", || {
        b.read("after.txt").as_deref() == Some(b"not for c".as_slice())
    })
    .await;
    // One rotation, or two at once: never a third generation.
    assert_eq!(generation(&engine_a), Some(2));
    assert_eq!(generation(&engine_c), Some(1));
    assert!(c.read("after.txt").is_none());

    engine_c.shutdown().await;
    engine_b.shutdown().await;
    engine_a.shutdown().await;
}

/// Removing a user from the members, here by editing the folder's configuration, rotates the
/// key on the device that removes them, and the removed user's devices are not granted the new
/// one (ADR 0023).
#[tokio::test(flavor = "multi_thread")]
async fn removing_a_member_rotates_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let owner = UserKey::from_seed([8; 32]);
    let guest = UserKey::from_seed([9; 32]);
    let members = [owner.id(), guest.id()];
    let a = Member::new(dir.path(), "a", 80);
    let g = Member::new(dir.path(), "g", 81);
    let (id_a, id_g) = (a.key.id(), g.key.id());
    let engine_a = a.start_with(&owner, &members, &[], &network, &[id_g]).await;
    let engine_g = g.start_with(&guest, &members, &[], &network, &[id_a]).await;
    a.write("shared.txt", b"for both");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("the guest has the shared file", || {
        g.read("shared.txt").as_deref() == Some(b"for both".as_slice())
    })
    .await;

    engine_a.shutdown().await;
    let engine_a = a
        .start_with(&owner, &[owner.id()], &[], &network, &[id_g])
        .await;
    assert_eq!(generation(&engine_a), Some(2));
    a.write("later.txt", b"owner only");
    engine_a.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(generation(&engine_g), Some(1));
    assert!(g.read("later.txt").is_none());

    engine_g.shutdown().await;
    engine_a.shutdown().await;
}
