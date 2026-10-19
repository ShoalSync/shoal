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
                    stored: None,
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
    // c gets a's sealed versions but cannot open them: they wait, unopened, for a key.
    eventually("c keeps a's sealed version", || {
        engine_c
            .folders()
            .into_iter()
            .find(|f| f.id == FOLDER)
            .and_then(|f| f.encryption)
            .is_some_and(|e| e.waiting_versions == 1)
    })
    .await;
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

/// Starts a storage peer of `operator`'s for the folder, stored for `members` (ADR 0023): no
/// files, no keys, only sealed versions and items.
async fn storage_peer(
    dir: &Path,
    seed: u8,
    operator: &UserKey,
    members: &[UserId],
    network: &Network,
    peers: &[DeviceId],
    quota_bytes: u64,
) -> (DeviceKey, Engine) {
    let key = DeviceKey::from_seed([seed; 32]);
    let data = dir.join("storage").join("data");
    let config = EngineConfig {
        data_dir: data.clone(),
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
            path: data.join("stored").join(FOLDER.to_string()),
            mode: ReplicaMode::Full,
            members: members.to_vec(),
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(3600),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: Some(Encryption {
                storage_peers: Vec::new(),
                stored: Some(tidefell_engine::Stored { quota_bytes }),
            }),
        }],
        min_free_space: None,
    };
    let certificate = operator.certify(key.id(), "storage", 0);
    let engine = Engine::start_simulated(config, key.clone(), certificate, network)
        .await
        .unwrap();
    engine.set_retry_interval(Duration::from_millis(50));
    (key, engine)
}

fn stored(engine: &Engine) -> tidefell_engine::StoredInfo {
    engine
        .folders()
        .into_iter()
        .find(|f| f.id == FOLDER)
        .and_then(|f| f.encryption)
        .and_then(|e| e.stored)
        .unwrap()
}

/// Two members that never meet sync an encrypted folder through a storage peer (ADR 0023): the
/// second gets the folder key granted by the first, relayed, opens the first's versions and
/// fetches their content from it, while the storage peer holds them sealed, keeps no key and no
/// plaintext, and serves only the newest version of each entry.
#[tokio::test(flavor = "multi_thread")]
async fn members_meet_through_a_storage_peer() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([10; 32]);
    let operator = UserKey::from_seed([11; 32]);
    let a = Member::new(dir.path(), "a", 100);
    let b = Member::new(dir.path(), "b", 101);
    let (id_a, id_b) = (a.key.id(), b.key.id());
    let storage_key = DeviceKey::from_seed([102; 32]).id();
    let (_, engine_s) = storage_peer(
        dir.path(),
        102,
        &operator,
        &[user.id()],
        &network,
        &[id_a, id_b],
        1 << 30,
    )
    .await;
    // a and b each know only the storage peer.
    let engine_a = a
        .start_with(
            &user,
            &[user.id()],
            &[storage_key],
            &network,
            &[storage_key],
        )
        .await;
    a.write("notes.txt", b"first");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("the storage peer keeps a's version", || {
        stored(&engine_s).versions == 1
    })
    .await;
    // Nothing of the folder is on the storage peer's disk as files, and it holds no key.
    assert_eq!(engine_s.folder_key_generation(FOLDER).unwrap(), None);
    assert!(engine_s.files(FOLDER).unwrap().is_empty());

    let engine_b = b
        .start_with(
            &user,
            &[user.id()],
            &[storage_key],
            &network,
            &[storage_key],
        )
        .await;
    eventually("b gets a's file through the storage peer", || {
        b.read("notes.txt").as_deref() == Some(b"first".as_slice())
    })
    .await;
    assert_eq!(generation(&engine_b), Some(1));
    let s = stored(&engine_s);
    assert!(s.bytes > 0);
    assert_eq!(s.missing_chunks, 0);

    // An edit supersedes the version the storage peer had: it keeps that one, unserved.
    a.write("notes.txt", b"second");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("the storage peer supersedes the first version", || {
        let s = stored(&engine_s);
        s.versions == 1 && s.superseded == 1
    })
    .await;
    eventually("b gets the edit", || {
        b.read("notes.txt").as_deref() == Some(b"second".as_slice())
    })
    .await;
    // Nowhere on the storage peer's disk is the plaintext of either version.
    for file in walk(&dir.path().join("storage")) {
        let bytes = fs::read(&file).unwrap();
        for plain in [b"first".as_slice(), b"second".as_slice()] {
            assert!(
                !bytes.windows(plain.len()).any(|w| w == plain),
                "{} holds plaintext",
                file.display()
            );
        }
    }

    // What it stores outlasts a restart.
    engine_s.shutdown().await;
    let (_, engine_s) = storage_peer(
        dir.path(),
        102,
        &operator,
        &[user.id()],
        &network,
        &[id_a, id_b],
        1 << 30,
    )
    .await;
    let s = stored(&engine_s);
    assert_eq!((s.versions, s.superseded), (1, 1));
    // Past the folder's retention, the superseded version goes, and the chunks only it listed;
    // the current one stays whole.
    let before = stored(&engine_s).bytes;
    engine_s.purge(engine_s.now_ms() + 31 * 24 * 60 * 60 * 1000);
    let s = stored(&engine_s);
    assert_eq!((s.versions, s.superseded, s.missing_chunks), (1, 0, 0));
    assert!(s.bytes < before);

    engine_b.shutdown().await;
    engine_a.shutdown().await;
    engine_s.shutdown().await;
}

/// Every file under `dir`.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(walk(&path));
        } else {
            files.push(path);
        }
    }
    files
}

/// A storage peer stops fetching once its quota is reached, and says what it lacks, rather
/// than drop what it holds (ADR 0023).
#[tokio::test(flavor = "multi_thread")]
async fn a_storage_peer_stops_at_its_quota() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([12; 32]);
    let operator = UserKey::from_seed([13; 32]);
    let a = Member::new(dir.path(), "a", 110);
    let storage_key = DeviceKey::from_seed([111; 32]).id();
    // Room for less than one chunk: the first fills it.
    let (_, engine_s) = storage_peer(
        dir.path(),
        111,
        &operator,
        &[user.id()],
        &network,
        &[a.key.id()],
        1,
    )
    .await;
    let engine_a = a
        .start_with(
            &user,
            &[user.id()],
            &[storage_key],
            &network,
            &[storage_key],
        )
        .await;
    a.write("one.txt", b"the first file");
    a.write("two.txt", b"the second file");
    engine_a.scan(FOLDER).await.unwrap();
    eventually(
        "the storage peer keeps both versions and fills its quota",
        || {
            let s = stored(&engine_s);
            s.versions == 2 && s.bytes > 0
        },
    )
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let s = stored(&engine_s);
    assert_eq!(
        s.missing_chunks, 1,
        "one file's chunk waits for room: {s:?}"
    );

    engine_a.shutdown().await;
    engine_s.shutdown().await;
}

/// A file of more than 4096 chunks lists them in list blobs (ADR 0023): a member seals it so,
/// the storage peer fetches its list blobs and then its chunks, and a member that meets the
/// author only through the storage peer opens it and gets it whole. About a gigabyte of data,
/// so run by hand: `cargo test --release --features simulation --test encrypted -- --ignored`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "writes and syncs over a gigabyte"]
async fn a_file_of_more_than_4096_chunks_syncs_through_list_blobs() {
    use std::io::Write as _;

    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([14; 32]);
    let operator = UserKey::from_seed([15; 32]);
    let a = Member::new(dir.path(), "a", 120);
    let b = Member::new(dir.path(), "b", 121);
    let (id_a, id_b) = (a.key.id(), b.key.id());
    let storage_key = DeviceKey::from_seed([122; 32]).id();
    let (_, engine_s) = storage_peer(
        dir.path(),
        122,
        &operator,
        &[user.id()],
        &network,
        &[id_a, id_b],
        u64::MAX,
    )
    .await;
    let engine_a = a
        .start_with(
            &user,
            &[user.id()],
            &[storage_key],
            &network,
            &[storage_key],
        )
        .await;
    // Random bytes chunk at 256 KiB on average: 1.1 GiB is well over 4096 chunks.
    let big = a.root.join("big.bin");
    fs::create_dir_all(&a.root).unwrap();
    let mut file = std::io::BufWriter::new(fs::File::create(&big).unwrap());
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut block = vec![0u8; 1 << 20];
    for _ in 0..1100 {
        for word in block.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            word.copy_from_slice(&state.to_le_bytes());
        }
        file.write_all(&block).unwrap();
    }
    file.flush().unwrap();
    drop(file);
    let want = blake3::hash(&fs::read(&big).unwrap());
    engine_a.scan(FOLDER).await.unwrap();
    eventually_for("the storage peer stores all of it", 600, || {
        let s = stored(&engine_s);
        s.versions == 1 && s.missing_chunks == 0 && s.bytes > 1 << 30
    })
    .await;

    let engine_b = b
        .start_with(
            &user,
            &[user.id()],
            &[storage_key],
            &network,
            &[storage_key],
        )
        .await;
    eventually_for(
        "b gets the whole file through the storage peer",
        600,
        || {
            b.read("big.bin")
                .is_some_and(|bytes| blake3::hash(&bytes) == want)
        },
    )
    .await;

    engine_b.shutdown().await;
    engine_a.shutdown().await;
    engine_s.shutdown().await;
}

async fn eventually_for(what: &str, seconds: u64, mut check: impl FnMut() -> bool) {
    for _ in 0..seconds * 10 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

fn only_here(engine: &Engine) -> usize {
    engine
        .folders()
        .into_iter()
        .find(|f| f.id == FOLDER)
        .unwrap()
        .only_here
}

/// A storage peer's copy is a confirmed copy only while a member device other than the one
/// asking holds a grant of the key (ADR 0023, ADR 0022): otherwise losing that device would lose
/// the key, and with it everything the storage peer holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_storage_peers_copy_counts_once_another_member_holds_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let network = Network::new();
    let user = UserKey::from_seed([16; 32]);
    let operator = UserKey::from_seed([17; 32]);
    let a = Member::new(dir.path(), "a", 130);
    let b = Member::new(dir.path(), "b", 131);
    let (id_a, id_b) = (a.key.id(), b.key.id());
    let storage_key = DeviceKey::from_seed([132; 32]).id();
    let (_, engine_s) = storage_peer(
        dir.path(),
        132,
        &operator,
        &[user.id()],
        &network,
        &[id_a, id_b],
        1 << 30,
    )
    .await;
    let engine_a = a
        .start_with(
            &user,
            &[user.id()],
            &[storage_key],
            &network,
            &[storage_key],
        )
        .await;
    a.write("photo.jpg", b"only here, as far as a can tell");
    engine_a.scan(FOLDER).await.unwrap();
    eventually("the storage peer holds the photo", || {
        stored(&engine_s).missing_chunks == 0 && stored(&engine_s).versions == 1
    })
    .await;
    // Held, but a alone has the key: not a copy that counts.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(only_here(&engine_a), 1);

    // Once a has granted the key to b, the storage peer's copy counts.
    let engine_b = b
        .start_with(
            &user,
            &[user.id()],
            &[storage_key],
            &network,
            &[storage_key],
        )
        .await;
    eventually("b is granted the key", || generation(&engine_b) == Some(1)).await;
    eventually("the storage peer's copy counts", || {
        only_here(&engine_a) == 0
    })
    .await;

    engine_b.shutdown().await;
    engine_a.shutdown().await;
    engine_s.shutdown().await;
}
