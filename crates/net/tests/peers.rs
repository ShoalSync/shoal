//! Multi-peer tests with in-process iroh endpoints on localhost and relays disabled.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tidefell_identity::{DeviceKey, FolderMembers, UserKey};
use tidefell_meta::{Folder, Retention};
use tidefell_net::{Backend, ChunkResult, IndexPage, Node, NodeConfig, RelayConfig};
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

    fn current(&self, entry: u8) -> Option<EntryVersion> {
        self.folder
            .lock()
            .unwrap()
            .current(&EntryId([entry; 16]))
            .cloned()
    }

    fn config(&self) -> NodeConfig {
        NodeConfig {
            secret_key: self.key.seed(),
            relay: RelayConfig::Disabled,
            address_lookup: false,
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

    fn index_since(&self, folder: &FolderId, epoch: u64, since: u64) -> Option<IndexPage> {
        if *folder != FOLDER {
            return None;
        }
        let folder = self.folder.lock().unwrap();
        let (versions, up_to) = folder.index_since(epoch, since);
        Some(IndexPage {
            epoch: folder.epoch(),
            up_to,
            versions: versions.into_iter().map(|s| s.signed.clone()).collect(),
        })
    }

    fn apply_versions(&self, _: &FolderId, _: DeviceId, versions: Vec<Signed<EntryVersion>>) {
        let mut changed = false;
        {
            let mut folder = self.folder.lock().unwrap();
            for v in versions {
                if let Ok(applied) = folder.apply_remote(v) {
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
