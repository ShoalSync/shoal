//! Multi-peer tests with in-process iroh endpoints on localhost and relays disabled.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tidefell_identity::{DeviceKey, FolderMembers, UserKey};
use tidefell_meta::{Folder, Retention};
use tidefell_net::{
    Backend, ChunkResult, IndexPage, Limits, Node, NodeConfig, RelayConfig, MIN_BYTES_PER_SEC,
};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::{
    ChunkHash, ChunkRef, ContentHash, DeviceId, EntryId, FolderId, Manifest, Signed,
};
use tokio::sync::watch;

const FOLDER: FolderId = FolderId([7; 32]);

struct TestBackend {
    key: DeviceKey,
    cert: Signed<DeviceCertificate>,
    members: FolderMembers,
    folder: Mutex<Folder>,
    progress: Mutex<HashMap<(FolderId, DeviceId), (u64, u64)>>,
    manifests: Mutex<HashMap<ContentHash, Manifest>>,
    chunks: Mutex<HashMap<ChunkHash, Vec<u8>>>,
    changes: watch::Sender<u64>,
    /// Certificates known (ADR 0024).
    certificates: Mutex<HashMap<DeviceId, Signed<DeviceCertificate>>>,
    /// Certificate requests go unanswered: the stream is reset.
    silent: std::sync::atomic::AtomicBool,
}

impl TestBackend {
    fn new(seed: u8, user: &UserKey, members: &[&UserKey]) -> Arc<Self> {
        let key = DeviceKey::from_seed([seed; 32]);
        Arc::new(Self {
            cert: user.certify(key.id(), format!("device {seed}"), 0),
            members: FolderMembers::new(FOLDER, members.iter().map(|u| u.id())),
            folder: Mutex::new(Folder::new(
                FOLDER,
                key.id(),
                u64::from(seed),
                Retention::Forever,
            )),
            key,
            progress: Mutex::default(),
            manifests: Mutex::default(),
            chunks: Mutex::default(),
            changes: watch::channel(0).0,
            certificates: Mutex::default(),
            silent: Default::default(),
        })
    }

    fn bump(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    /// Creates or updates a single-chunk file.
    fn write_file(&self, entry: u8, path: &str, data: &[u8]) {
        let hash = ChunkHash(*blake3::hash(data).as_bytes());
        let content = ContentHash(*blake3::hash(data).as_bytes());
        self.chunks.lock().unwrap().insert(hash, data.to_vec());
        self.manifests.lock().unwrap().insert(
            content,
            Manifest {
                content,
                chunks: vec![ChunkRef {
                    hash,
                    len: data.len() as u32,
                }],
            },
        );
        self.folder
            .lock()
            .unwrap()
            .local_change(
                self.key.signing_key(),
                EntryId([entry; 16]),
                path,
                EntryKind::File {
                    content,
                    size: data.len() as u64,
                    executable: false,
                },
                1,
            )
            .unwrap();
        self.bump();
    }

    /// Records `count` small files, numbered from `start`, in one go.
    fn bulk(&self, start: u32, count: u32) {
        let mut folder = self.folder.lock().unwrap();
        for n in start..start + count {
            let mut id = [0u8; 16];
            id[..4].copy_from_slice(&n.to_le_bytes());
            folder
                .local_change(
                    self.key.signing_key(),
                    EntryId(id),
                    &format!("f{n}"),
                    EntryKind::File {
                        content: ContentHash([1; 32]),
                        size: 1,
                        executable: false,
                    },
                    1,
                )
                .unwrap();
        }
        drop(folder);
        self.bump();
    }

    fn entry_count(&self) -> usize {
        self.folder.lock().unwrap().entries().count()
    }

    fn current(&self, entry: u8) -> Option<EntryVersion> {
        self.folder
            .lock()
            .unwrap()
            .current(&EntryId([entry; 16]))
            .map(|v| v.to_version())
    }

    fn config(&self) -> NodeConfig {
        NodeConfig {
            secret_key: self.key.seed(),
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            limits: Default::default(),
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        }
    }
}

impl Backend for TestBackend {
    fn certificate(&self) -> Signed<DeviceCertificate> {
        self.cert.clone()
    }

    fn folders(&self) -> Vec<FolderId> {
        vec![FOLDER]
    }

    fn is_member(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool {
        *folder == FOLDER && self.members.contains(&peer.user)
    }

    fn index_since(
        &self,
        folder: &FolderId,
        epoch: u64,
        since: u64,
        limit: usize,
    ) -> Option<IndexPage> {
        if *folder != FOLDER {
            return None;
        }
        let folder = self.folder.lock().unwrap();
        let (versions, up_to) = folder.index_since(epoch, since, limit);
        Some(IndexPage {
            epoch: folder.epoch(),
            up_to,
            versions: versions
                .into_iter()
                .filter_map(|s| s.signed.cloned())
                .collect(),
        })
    }

    fn certificate_of(&self, _: &FolderId, device: DeviceId) -> Option<Signed<DeviceCertificate>> {
        assert!(
            !self.silent.load(std::sync::atomic::Ordering::Relaxed),
            "this peer does not answer"
        );
        if device == self.key.id() {
            return Some(self.cert.clone());
        }
        self.certificates.lock().unwrap().get(&device).cloned()
    }
    fn learn_certificate(&self, certificate: Signed<DeviceCertificate>) {
        let device = certificate.verify_certificate().unwrap().device;
        self.certificates
            .lock()
            .unwrap()
            .insert(device, certificate);
    }
    fn unknown_authors(&self, versions: &[Signed<EntryVersion>]) -> Vec<DeviceId> {
        let certificates = self.certificates.lock().unwrap();
        let mut authors: Vec<DeviceId> = versions
            .iter()
            .map(|v| v.verify().unwrap().author)
            .filter(|a| *a != self.key.id() && !certificates.contains_key(a))
            .collect();
        authors.dedup();
        authors
    }
    fn apply_versions(&self, _: &FolderId, _: DeviceId, versions: Vec<Signed<EntryVersion>>) {
        let mut changed = false;
        {
            let mut folder = self.folder.lock().unwrap();
            for v in versions {
                if let Ok(applied) = folder.apply_remote(v, 0) {
                    changed |= applied != tidefell_meta::Applied::Stale;
                }
            }
        }
        if changed {
            self.bump();
        }
    }

    fn peer_progress(&self, folder: &FolderId, peer: DeviceId) -> (u64, u64) {
        self.progress
            .lock()
            .unwrap()
            .get(&(*folder, peer))
            .copied()
            .unwrap_or((0, 0))
    }

    fn set_peer_progress(&self, folder: &FolderId, peer: DeviceId, epoch: u64, up_to: u64) {
        self.progress
            .lock()
            .unwrap()
            .insert((*folder, peer), (epoch, up_to));
    }

    fn manifest(&self, _: &FolderId, content: &ContentHash) -> Option<Manifest> {
        self.manifests.lock().unwrap().get(content).cloned()
    }

    fn chunk(&self, _: &FolderId, hash: &ChunkHash) -> Option<Vec<u8>> {
        let referenced = self
            .manifests
            .lock()
            .unwrap()
            .values()
            .any(|m| m.chunks.iter().any(|c| c.hash == *hash));
        referenced
            .then(|| self.chunks.lock().unwrap().get(hash).cloned())
            .flatten()
    }

    fn availability(&self, _: &FolderId, content: &ContentHash) -> Option<(u32, Vec<u8>)> {
        let manifest = self.manifests.lock().unwrap().get(content).cloned()?;
        let chunks = self.chunks.lock().unwrap();
        let mut bitmap = vec![0u8; manifest.chunks.len().div_ceil(8)];
        for (i, c) in manifest.chunks.iter().enumerate() {
            if chunks.contains_key(&c.hash) {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        Some((manifest.chunks.len() as u32, bitmap))
    }

    fn is_full_replica(&self, folder: &FolderId) -> Option<bool> {
        (*folder == FOLDER).then_some(true)
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn content_of(v: &EntryVersion) -> ContentHash {
    match v.kind {
        EntryKind::File { content, .. } => content,
        _ => panic!("not a file"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn index_sync_and_live_push() {
    let alice = UserKey::from_seed([100; 32]);
    let a = TestBackend::new(1, &alice, &[&alice]);
    let b = TestBackend::new(2, &alice, &[&alice]);
    a.write_file(1, "before.txt", b"written before connecting");

    let node_a = Node::spawn(a.config(), a.clone()).await.unwrap();
    let node_b = Node::spawn(b.config(), b.clone()).await.unwrap();
    let peer = node_b.connect(node_a.addr()).await.unwrap();
    assert_eq!(peer.device(), node_a.id());
    assert!(peer.shared_folders().contains(&FOLDER));

    eventually("initial index", || b.current(1).is_some()).await;
    assert_eq!(b.current(1).unwrap().path, "before.txt");

    // Live push in both directions over the existing session.
    b.write_file(2, "from-b.txt", b"b's file");
    eventually("push from b", || a.current(2).is_some()).await;
    a.write_file(3, "from-a.txt", b"a's file");
    eventually("push from a", || b.current(3).is_some()).await;
    a.write_file(1, "before.txt", b"edited");
    eventually("edit from a", || {
        b.current(1)
            .is_some_and(|v| v.version.get(&node_a.id()) == 2)
    })
    .await;

    // b recorded progress against a's index.
    assert!(b.peer_progress(&FOLDER, node_a.id()).1 > 0);

    node_b.shutdown().await;
    node_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn manifests_chunks_and_availability() {
    let alice = UserKey::from_seed([100; 32]);
    let a = TestBackend::new(1, &alice, &[&alice]);
    let b = TestBackend::new(2, &alice, &[&alice]);
    a.write_file(1, "data.bin", b"chunk payload");

    let node_a = Node::spawn(a.config(), a.clone()).await.unwrap();
    let node_b = Node::spawn(b.config(), b.clone()).await.unwrap();
    let peer = node_b.connect(node_a.addr()).await.unwrap();
    eventually("index", || b.current(1).is_some()).await;
    let content = content_of(&b.current(1).unwrap());

    let manifest = peer.request_manifest(FOLDER, content).await.unwrap();
    assert_eq!(manifest.content, content);
    let wanted = manifest.chunks[0].hash;
    assert!(peer
        .request_manifest(FOLDER, ContentHash([0; 32]))
        .await
        .is_err());

    // With a sync session open, the peer can be dialed by ID alone (no address lookup).
    // This is the first blobs/1 connection, so nothing is cached.
    let by_id = tidefell_net::EndpointId::from_bytes(node_a.id().as_bytes()).unwrap();
    let results = node_b.fetch_chunks(by_id, FOLDER, &[wanted]).await.unwrap();
    assert!(matches!(results[0], ChunkResult::Found(..)));

    let unknown = ChunkHash([9; 32]);
    let results = node_b
        .fetch_chunks(node_a.addr(), FOLDER, &[wanted, unknown])
        .await
        .unwrap();
    assert_eq!(
        results,
        vec![
            ChunkResult::Found(wanted, b"chunk payload".to_vec()),
            ChunkResult::Missing(unknown)
        ]
    );

    let (count, bitmap) = node_b
        .availability(node_a.addr(), FOLDER, content)
        .await
        .unwrap();
    assert_eq!((count, bitmap), (1, vec![1]));

    assert!(node_b.is_full_replica(node_a.addr(), FOLDER).await.unwrap());
    assert!(node_b
        .is_full_replica(node_a.addr(), FolderId([0x99; 32]))
        .await
        .is_err());

    node_b.shutdown().await;
    node_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn certificates_of_unknown_authors_are_fetched() {
    let alice = UserKey::from_seed([100; 32]);
    let a = TestBackend::new(1, &alice, &[&alice]);
    let b = TestBackend::new(2, &alice, &[&alice]);
    // A third device of alice's wrote a file that a holds: a knows its certificate, b does not.
    let c = DeviceKey::from_seed([3; 32]);
    let mut by_c = Folder::new(FOLDER, c.id(), 3, Retention::Forever);
    let version = by_c
        .local_change(
            c.signing_key(),
            EntryId([9; 16]),
            "by-c.txt",
            EntryKind::File {
                content: ContentHash([1; 32]),
                size: 1,
                executable: false,
            },
            0,
        )
        .unwrap();
    a.folder.lock().unwrap().apply_remote(version, 0).unwrap();
    a.learn_certificate(alice.certify(c.id(), "device 3", 0));
    a.bump();

    let node_a = Node::spawn(a.config(), a.clone()).await.unwrap();
    let node_b = Node::spawn(b.config(), b.clone()).await.unwrap();
    let peer = node_b.connect(node_a.addr()).await.unwrap();
    eventually("b has c's version", || b.current(9).is_some()).await;
    eventually("b fetched c's certificate from a", || {
        b.certificates.lock().unwrap().contains_key(&c.id())
    })
    .await;
    assert!(
        b.certificates.lock().unwrap().contains_key(&a.key.id()),
        "a's, from the handshake"
    );
    assert!(
        a.certificates.lock().unwrap().contains_key(&b.key.id()),
        "and b's on a"
    );
    // A device nobody knows gets an error, not a certificate.
    let unknown = peer.request_certificate(FOLDER, DeviceId([0x55; 32])).await;
    assert!(
        matches!(unknown, Err(tidefell_net::Error::Rejected(ref code)) if code == "UnknownDevice"),
        "{unknown:?}"
    );

    node_b.shutdown().await;
    node_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unanswered_certificate_request_leaves_the_index_unacknowledged() {
    let alice = UserKey::from_seed([100; 32]);
    let a = TestBackend::new(1, &alice, &[&alice]);
    let b = TestBackend::new(2, &alice, &[&alice]);
    // A holds a version by C, a device B does not know, and does not answer B about it.
    let c = DeviceKey::from_seed([3; 32]);
    let mut by_c = Folder::new(FOLDER, c.id(), 3, Retention::Forever);
    let version = by_c
        .local_change(
            c.signing_key(),
            EntryId([9; 16]),
            "by-c.txt",
            EntryKind::File {
                content: ContentHash([1; 32]),
                size: 1,
                executable: false,
            },
            0,
        )
        .unwrap();
    a.folder.lock().unwrap().apply_remote(version, 0).unwrap();
    a.learn_certificate(alice.certify(c.id(), "device 3", 0));
    a.silent.store(true, std::sync::atomic::Ordering::Relaxed);
    a.bump();

    let node_a = Node::spawn(a.config(), a.clone()).await.unwrap();
    let node_b = Node::spawn(b.config(), b.clone()).await.unwrap();
    node_b.connect(node_a.addr()).await.unwrap();
    // This backend applies anything; what matters is that B does not record A's index as
    // received, so A sends it again next session (ADR 0024).
    eventually("b has c's version", || b.current(9).is_some()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!b
        .progress
        .lock()
        .unwrap()
        .contains_key(&(FOLDER, a.key.id())));
    node_b.shutdown().await;

    // Answered next time: the index is acknowledged.
    a.silent.store(false, std::sync::atomic::Ordering::Relaxed);
    let node_b = Node::spawn(b.config(), b.clone()).await.unwrap();
    node_b.connect(node_a.addr()).await.unwrap();
    eventually("b acknowledges a's index", || {
        b.progress
            .lock()
            .unwrap()
            .contains_key(&(FOLDER, a.key.id()))
    })
    .await;
    assert!(b.certificates.lock().unwrap().contains_key(&c.id()));

    node_b.shutdown().await;
    node_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn non_members_get_nothing() {
    let alice = UserKey::from_seed([100; 32]);
    let mallory = UserKey::from_seed([101; 32]);
    let a = TestBackend::new(1, &alice, &[&alice]);
    // Mallory's device believes it may sync the folder, but a only admits alice.
    let m = TestBackend::new(3, &mallory, &[&alice, &mallory]);
    a.write_file(1, "secret.txt", b"secret");
    let content = content_of(&a.current(1).unwrap());
    let chunk = ChunkHash(*blake3::hash(b"secret").as_bytes());

    let node_a = Node::spawn(a.config(), a.clone()).await.unwrap();
    let node_m = Node::spawn(m.config(), m.clone()).await.unwrap();
    let peer = node_m.connect(node_a.addr()).await.unwrap();
    assert!(peer.shared_folders().is_empty());
    assert!(peer.request_manifest(FOLDER, content).await.is_err());
    assert!(node_m
        .fetch_chunks(node_a.addr(), FOLDER, &[chunk])
        .await
        .is_err());
    assert!(node_m
        .availability(node_a.addr(), FOLDER, content)
        .await
        .is_err());
    assert!(node_m.is_full_replica(node_a.addr(), FOLDER).await.is_err());

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(m.current(1).is_none(), "no index leaked to a non-member");

    node_m.shutdown().await;
    node_a.shutdown().await;
}

fn new_device_config(seed: u8) -> NodeConfig {
    NodeConfig {
        secret_key: [seed; 32],
        relay: RelayConfig::Disabled,
        address_lookup: false,
        local_discovery: false,
        limits: Default::default(),
        bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
    }
}

async fn next_event(server: &mut tidefell_net::PairingServer) -> tidefell_net::PairingEvent {
    tokio::time::timeout(Duration::from_secs(10), server.next_event())
        .await
        .expect("pairing event")
        .expect("server running")
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_issues_a_certificate_once() {
    use tidefell_net::{PairingEvent, PairingServer};
    use tidefell_proto::pair::FolderOffer;

    let alice = UserKey::from_seed([1; 32]);
    let laptop = TestBackend::new(10, &alice, &[&alice]);
    let node = Node::spawn(laptop.config(), laptop.clone()).await.unwrap();
    let phone_key = DeviceKey::from_seed([50; 32]);
    let mut server = PairingServer::start(
        new_device_config(50),
        "Pixel".into(),
        Duration::from_secs(600),
    )
    .await
    .unwrap();
    let code = server.code().clone();
    assert_eq!(code.device, phone_key.id());

    // A wrong secret is refused and reported; the code stays usable.
    let mut wrong = code.clone();
    wrong.secret[0] ^= 1;
    assert!(node.pair(&wrong).await.is_err());
    assert!(matches!(
        next_event(&mut server).await,
        PairingEvent::Failed { .. }
    ));

    // Declining ends that attempt only.
    let session = node.pair(&code).await.unwrap();
    let PairingEvent::Confirming {
        confirmation_code, ..
    } = next_event(&mut server).await
    else {
        panic!("expected confirming");
    };
    assert_eq!(confirmation_code, session.confirmation_code());
    session.decline().await.unwrap();
    assert!(matches!(
        next_event(&mut server).await,
        PairingEvent::Failed { .. }
    ));

    // The real pairing.
    let session = node.pair(&code).await.unwrap();
    assert_eq!(session.suggested_name(), "Pixel");
    assert_eq!(session.device(), phone_key.id());
    let PairingEvent::Confirming {
        approver,
        confirmation_code,
    } = next_event(&mut server).await
    else {
        panic!("expected confirming");
    };
    assert_eq!(approver.user, alice.id());
    assert_eq!(confirmation_code, session.confirmation_code());
    let offer = FolderOffer {
        id: FOLDER,
        name: "Photos".into(),
        members: vec![alice.id()],
        retention_days: 30,
    };
    session
        .complete(
            alice.certify(phone_key.id(), "Alice's phone", 1),
            &node.addr(),
            vec![offer.clone()],
        )
        .await
        .unwrap();
    let PairingEvent::Paired(paired) = next_event(&mut server).await else {
        panic!("expected paired");
    };
    let certificate = paired.certificate.verify_certificate().unwrap();
    assert_eq!(certificate.device, phone_key.id());
    assert_eq!(certificate.name, "Alice's phone");
    assert_eq!(paired.approver.device, laptop.key.id());
    assert_eq!(*paired.approver_addr.id.as_bytes(), laptop.key.id().0);
    assert!(paired.approver_addr.ip_addrs().next().is_some());
    assert_eq!(paired.folders, vec![offer]);

    // The code is used up.
    assert!(node.pair(&code).await.is_err());

    // A certificate for another device is rejected by a fresh code.
    let mut server = PairingServer::start(
        new_device_config(51),
        "Other".into(),
        Duration::from_secs(600),
    )
    .await
    .unwrap();
    let session = node.pair(server.code()).await.unwrap();
    assert!(session
        .complete(
            alice.certify(phone_key.id(), "wrong device", 1),
            &node.addr(),
            Vec::new(),
        )
        .await
        .is_err());
    let _confirming = next_event(&mut server).await;
    assert!(matches!(
        next_event(&mut server).await,
        PairingEvent::Failed { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn large_indexes_sync_in_both_directions() {
    // ADR 0024: both sides used to block writing their whole index while neither read, so a
    // first sync of tens of thousands of versions never finished.
    // Each replica ends with 100,000 entries, the Phase 5c exit criterion.
    const EACH: u32 = 50_000;
    let alice = UserKey::from_seed([103; 32]);
    let a = TestBackend::new(7, &alice, &[&alice]);
    let b = TestBackend::new(8, &alice, &[&alice]);
    a.bulk(0, EACH);
    b.bulk(EACH, EACH);

    let node_a = Node::spawn(a.config(), a.clone()).await.unwrap();
    let node_b = Node::spawn(b.config(), b.clone()).await.unwrap();
    node_b.connect(node_a.addr()).await.unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    while a.entry_count() < 2 * EACH as usize || b.entry_count() < 2 * EACH as usize {
        assert!(
            std::time::Instant::now() < deadline,
            "stalled at a={} b={} of {}",
            a.entry_count(),
            b.entry_count(),
            2 * EACH
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    node_b.shutdown().await;
    node_a.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_that_never_answers_cannot_hold_a_connect() {
    // ADR 0024: a handshake must complete within 10 seconds.
    use iroh::endpoint::presets;
    let silent = iroh::Endpoint::builder(presets::N0)
        .secret_key(iroh::SecretKey::from_bytes(&[104; 32]))
        .alpns(vec![tidefell_proto::ALPN_SYNC_V1.to_vec()])
        .relay_mode(iroh::RelayMode::Disabled)
        .clear_address_lookup()
        .bind_addr(SocketAddr::from(([127, 0, 0, 1], 0)))
        .unwrap()
        .bind()
        .await
        .unwrap();
    let held = Arc::new(Mutex::new(Vec::new()));
    let accepting = {
        let (silent, held) = (silent.clone(), held.clone());
        tokio::spawn(async move {
            while let Some(incoming) = silent.accept().await {
                let Ok(conn) = incoming.await else { continue };
                // Take the stream and never answer.
                if let Ok(stream) = conn.accept_bi().await {
                    held.lock().unwrap().push((conn, stream));
                }
            }
        })
    };

    let alice = UserKey::from_seed([105; 32]);
    let a = TestBackend::new(9, &alice, &[&alice]);
    let node = Node::spawn(a.config(), a.clone()).await.unwrap();
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(60), node.connect(silent.addr()))
        .await
        .expect("connect gave up on its own");
    assert!(result.is_err(), "a silent peer is not a peer");
    let waited = started.elapsed();
    assert!(waited < Duration::from_secs(20), "gave up after {waited:?}");

    accepting.abort();
    node.shutdown().await;
    silent.close().await;
}

/// `count` files of `size` bytes of distinct data on `backend`, one chunk each; their hashes.
fn chunky_files(backend: &TestBackend, count: u8, size: usize) -> Vec<ChunkHash> {
    (0..count)
        .map(|n| {
            let data: Vec<u8> = (0..size).map(|i| (i as u8) ^ n.wrapping_mul(31)).collect();
            backend.write_file(n + 1, &format!("f{n}.bin"), &data);
            ChunkHash(*blake3::hash(&data).as_bytes())
        })
        .collect()
}

fn limited(backend: &TestBackend, limits: Limits) -> NodeConfig {
    NodeConfig {
        limits,
        ..backend.config()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sender_over_its_upload_limit_answers_busy() {
    let alice = UserKey::from_seed([110; 32]);
    let a = TestBackend::new(1, &alice, &[&alice]);
    let b = TestBackend::new(2, &alice, &[&alice]);
    // Four requests of 256 KiB chunks at the lowest limit queue 16 seconds of sending at once,
    // more than the backlog may hold.
    let chunks = chunky_files(&a, 12, 256 * 1024);
    let limits = Limits {
        upload: Some(MIN_BYTES_PER_SEC),
        download: None,
        // Loopback is local, which is otherwise not limited.
        limit_local: true,
    };
    let node_a = Node::spawn(limited(&a, limits), a.clone()).await.unwrap();
    let node_b = Node::spawn(b.config(), b.clone()).await.unwrap();
    assert_eq!(node_a.limits(), limits);

    // A single request is served chunk by chunk, each within its deadline. Several at once
    // build a backlog the sender cannot clear in time, so it answers the rest with Busy.
    let fetch = |group: &'static [usize]| {
        let wanted: Vec<ChunkHash> = group.iter().map(|&i| chunks[i]).collect();
        let (node_b, addr) = (&node_b, node_a.addr());
        async move { node_b.fetch_chunks(addr, FOLDER, &wanted).await.unwrap() }
    };
    let (r1, r2, r3, r4) = tokio::join!(
        fetch(&[0, 1, 2]),
        fetch(&[3, 4, 5]),
        fetch(&[6, 7, 8]),
        fetch(&[9, 10, 11])
    );
    let results: Vec<ChunkResult> = [r1, r2, r3, r4].concat();
    let found = results
        .iter()
        .filter(|r| matches!(r, ChunkResult::Found(..)))
        .count();
    let busy = results
        .iter()
        .filter(|r| matches!(r, ChunkResult::Busy(wait) if !wait.is_zero()))
        .count();
    assert!(
        busy >= 1,
        "no request was told the sender is busy; {found} found"
    );
    assert!(
        (1..12).contains(&found),
        "sent {found} before becoming busy"
    );

    // Without a limit, everything comes at once.
    node_a.set_limits(Limits::default());
    tokio::time::sleep(Duration::from_secs(1)).await;
    let results = node_b
        .fetch_chunks(node_a.addr(), FOLDER, &chunks)
        .await
        .unwrap();
    assert!(results.iter().all(|r| matches!(r, ChunkResult::Found(..))));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_receiver_paces_its_downloads() {
    let alice = UserKey::from_seed([111; 32]);
    let a = TestBackend::new(1, &alice, &[&alice]);
    let b = TestBackend::new(2, &alice, &[&alice]);
    let chunks = chunky_files(&a, 3, 128 * 1024);
    let node_a = Node::spawn(a.config(), a.clone()).await.unwrap();
    let limits = Limits {
        upload: None,
        download: Some(MIN_BYTES_PER_SEC),
        limit_local: true,
    };
    let node_b = Node::spawn(limited(&b, limits), b.clone()).await.unwrap();
    let start = std::time::Instant::now();
    let results = node_b
        .fetch_chunks(node_a.addr(), FOLDER, &chunks)
        .await
        .unwrap();
    assert!(results.iter().all(|r| matches!(r, ChunkResult::Found(..))));
    // 384 KiB at 64 KiB/s: the reads after the first two chunks wait about four seconds.
    assert!(
        start.elapsed() >= Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_requests_to_a_limited_sender_never_time_out() {
    let alice = UserKey::from_seed([112; 32]);
    let a = TestBackend::new(1, &alice, &[&alice]);
    let b = TestBackend::new(2, &alice, &[&alice]);
    let chunks = chunky_files(&a, 12, 256 * 1024);
    let limits = Limits {
        upload: Some(MIN_BYTES_PER_SEC),
        download: None,
        limit_local: true,
    };
    let node_a = Node::spawn(limited(&a, limits), a.clone()).await.unwrap();
    let node_b = Arc::new(Node::spawn(b.config(), b.clone()).await.unwrap());
    // Twelve single-chunk requests at once: the sender admits what it can send in time and
    // turns the rest away, so every request ends in a clean answer, none in a timeout.
    let mut tasks = tokio::task::JoinSet::new();
    for hash in chunks {
        let (node_b, addr) = (node_b.clone(), node_a.addr());
        tasks.spawn(async move { node_b.fetch_chunks(addr, FOLDER, &[hash]).await });
    }
    let (mut found, mut busy) = (0, 0);
    while let Some(result) = tasks.join_next().await {
        match result
            .unwrap()
            .expect("no request may fail or time out")
            .as_slice()
        {
            [ChunkResult::Found(..)] => found += 1,
            [ChunkResult::Busy(wait)] => {
                assert!(*wait >= Duration::from_secs(1));
                busy += 1;
            }
            other => panic!("unexpected answer: {} results", other.len()),
        }
    }
    assert!(found >= 1 && busy >= 1, "found {found}, busy {busy}");
}
