//! sync/2 and blobs/2 between in-process iroh endpoints, with an in-memory backend that stores
//! items as they come (`specs/sync-v2.md`, `specs/blobs-v2.md`). What the items mean is the
//! engine's; here they only have to travel, be asked about and be vouched for.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tidefell_identity::{DeviceKey, UserKey};
use tidefell_net::{
    Backend, BlobKind, BlobResult, EncryptedBackend, IndexPage, ItemPage, Node, NodeConfig,
    RelayConfig,
};
use tidefell_proto::blobs2::BlobId;
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::sync::EntryVersion;
use tidefell_proto::sync2::{ChunkList, EncryptionKey, Item, KeyGrant, SealedVersion};
use tidefell_proto::{
    ChunkHash, ChunkId, ContentHash, DeviceId, EntryId, FolderId, KeyId, Manifest, Signed, UserId,
};
use tokio::sync::watch;

const FOLDER: FolderId = FolderId([9; 32]);

struct Device {
    key: DeviceKey,
    cert: Signed<DeviceCertificate>,
    /// Member users of the folder; on a storage peer, its operator's list.
    members: Vec<UserId>,
    /// Devices this one lists as the folder's storage peers.
    storage_peers: Vec<DeviceId>,
    items: Mutex<Vec<(u64, Item)>>,
    blobs: Mutex<HashMap<BlobId, Vec<u8>>>,
    progress: Mutex<HashMap<(FolderId, DeviceId), (u64, u64)>>,
    certificates: Mutex<HashMap<DeviceId, Signed<DeviceCertificate>>>,
    cannot_vouch: Mutex<BTreeSet<DeviceId>>,
    /// Answers every certificate request that it cannot vouch.
    refuses: AtomicBool,
    changes: watch::Sender<u64>,
}

impl Device {
    fn new(seed: u8, user: &UserKey, members: &[&UserKey]) -> Arc<Self> {
        let key = DeviceKey::from_seed([seed; 32]);
        Arc::new(Self {
            cert: user.certify(key.id(), format!("device {seed}"), 0),
            key,
            members: members.iter().map(|u| u.id()).collect(),
            storage_peers: Vec::new(),
            items: Mutex::default(),
            blobs: Mutex::default(),
            progress: Mutex::default(),
            certificates: Mutex::default(),
            cannot_vouch: Mutex::default(),
            refuses: AtomicBool::new(false),
            changes: watch::channel(0).0,
        })
    }

    fn with_storage_peer(self: Arc<Self>, peer: DeviceId) -> Arc<Self> {
        let mut device = Arc::into_inner(self).unwrap();
        device.storage_peers.push(peer);
        Arc::new(device)
    }

    fn add(&self, item: Item) {
        let mut items = self.items.lock().unwrap();
        let sequence = items.last().map_or(1, |(s, _)| s + 1);
        items.push((sequence, item));
        drop(items);
        self.changes.send_modify(|n| *n += 1);
    }

    fn version(&self, entry: u8, chunks: Vec<ChunkId>) -> Item {
        Item::Version(Signed::sign(
            &SealedVersion {
                folder: FOLDER,
                entry: EntryId([entry; 16]),
                author: self.key.id(),
                generation: 1,
                key: KeyId([1; 16]),
                chunks: ChunkList::Inline(chunks),
                sealed: vec![entry; 104],
                version: [(self.key.id(), 1)].into_iter().collect(),
            },
            self.key.signing_key(),
        ))
    }

    fn encryption_key(&self) -> Item {
        Item::EncryptionKey(Signed::sign(
            &EncryptionKey {
                device: self.key.id(),
                key: [5; 32],
                issued_ms: 1,
            },
            self.key.signing_key(),
        ))
    }

    fn grant(&self, to: DeviceId) -> Item {
        Item::Grant(Signed::sign(
            &KeyGrant {
                folder: FOLDER,
                generation: 1,
                key: KeyId([1; 16]),
                recipient: to,
                recipient_key: [5; 32],
                enc: [6; 32],
                sealed: vec![7; 48],
            },
            self.key.signing_key(),
        ))
    }

    fn count(&self) -> usize {
        self.items.lock().unwrap().len()
    }

    fn signer(item: &Item) -> DeviceId {
        match item {
            Item::Version(v) => v.verify().unwrap().author,
            Item::EncryptionKey(k) => k.verify().unwrap().device,
            Item::Grant(g) => DeviceId(g.signer.0),
        }
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

    async fn node(self: &Arc<Self>) -> Node {
        Node::spawn_with(
            self.config(),
            self.clone() as Arc<dyn Backend>,
            Some(self.clone() as Arc<dyn EncryptedBackend>),
        )
        .await
        .unwrap()
    }
}

impl Backend for Device {
    fn certificate(&self) -> Signed<DeviceCertificate> {
        self.cert.clone()
    }
    fn folders(&self) -> Vec<FolderId> {
        Vec::new()
    }
    fn is_member(&self, _: &FolderId, _: &DeviceCertificate) -> bool {
        false
    }
    fn certificate_of(&self, _: &FolderId, device: DeviceId) -> Option<Signed<DeviceCertificate>> {
        if self.refuses.load(Ordering::Relaxed) {
            return None;
        }
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
    fn index_since(&self, _: &FolderId, _: u64, _: u64, _: usize) -> Option<IndexPage> {
        None
    }
    fn apply_versions(&self, _: &FolderId, _: DeviceId, _: Vec<Signed<EntryVersion>>) {}
    fn peer_progress(&self, _: &FolderId, _: DeviceId) -> (u64, u64) {
        (0, 0)
    }
    fn set_peer_progress(&self, _: &FolderId, _: DeviceId, _: u64, _: u64) {}
    fn manifest(&self, _: &FolderId, _: &ContentHash) -> Option<Manifest> {
        None
    }
    fn chunk(&self, _: &FolderId, _: &ChunkHash) -> Option<Vec<u8>> {
        None
    }
    fn availability(&self, _: &FolderId, _: &ContentHash) -> Option<(u32, Vec<u8>)> {
        None
    }
    fn is_full_replica(&self, _: &FolderId) -> Option<bool> {
        None
    }
    fn changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}

impl EncryptedBackend for Device {
    fn folders(&self) -> Vec<FolderId> {
        vec![FOLDER]
    }

    fn is_member(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool {
        *folder == FOLDER
            && (self.members.contains(&peer.user) || self.storage_peers.contains(&peer.device))
    }

    fn is_storage_peer(&self, _: &FolderId, peer: &DeviceCertificate) -> bool {
        !self.members.contains(&peer.user)
    }

    fn unknown_authors(&self, items: &[Item]) -> Vec<DeviceId> {
        let certificates = self.certificates.lock().unwrap();
        let mut authors: Vec<DeviceId> = items
            .iter()
            .map(Self::signer)
            .filter(|a| *a != self.key.id() && !certificates.contains_key(a))
            .collect();
        authors.sort();
        authors.dedup();
        authors
    }

    fn cannot_vouch(&self, _: &FolderId, author: DeviceId) {
        self.cannot_vouch.lock().unwrap().insert(author);
    }

    fn index_since(
        &self,
        folder: &FolderId,
        _epoch: u64,
        since: u64,
        limit: usize,
    ) -> Option<ItemPage> {
        if *folder != FOLDER {
            return None;
        }
        let items = self.items.lock().unwrap();
        let page: Vec<&(u64, Item)> = items
            .iter()
            .filter(|(s, _)| *s > since)
            .take(limit)
            .collect();
        let up_to = if page.len() == limit {
            page.last().map_or(since, |(s, _)| *s)
        } else {
            items.last().map_or(0, |(s, _)| *s)
        };
        Some(ItemPage {
            epoch: 1,
            up_to,
            items: page.into_iter().map(|(_, i)| i.clone()).collect(),
        })
    }

    fn apply_items(&self, _: &FolderId, _: DeviceId, items: Vec<Item>) {
        for item in items {
            let known = self.items.lock().unwrap().iter().any(|(_, i)| *i == item);
            let vouched = {
                let signer = Self::signer(&item);
                signer == self.key.id() || self.certificates.lock().unwrap().contains_key(&signer)
            };
            if !known && vouched {
                self.add(item);
            }
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

    fn blob(&self, _: &FolderId, id: &BlobId) -> Option<Vec<u8>> {
        self.blobs.lock().unwrap().get(id).cloned()
    }

    fn holds(&self, _: &FolderId, ids: &[BlobId]) -> Option<Vec<bool>> {
        let blobs = self.blobs.lock().unwrap();
        Some(ids.iter().map(|id| blobs.contains_key(id)).collect())
    }

    fn is_full_replica(&self, _: &FolderId) -> Option<bool> {
        Some(true)
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

#[tokio::test(flavor = "multi_thread")]
async fn items_of_every_kind_sync_and_push() {
    let user = UserKey::from_seed([1; 32]);
    let (a, b) = (
        Device::new(11, &user, &[&user]),
        Device::new(12, &user, &[&user]),
    );
    a.add(a.version(1, vec![ChunkId([3; 32])]));
    a.add(a.encryption_key());
    a.add(a.grant(b.key.id()));
    b.add(b.encryption_key());
    let (na, nb) = (a.node().await, b.node().await);

    let handle = na.connect_encrypted(nb.addr()).await.unwrap();
    assert!(handle.shared_folders().contains(&FOLDER));
    // A sync/2 session is its own: no sync/1 session, and no manifests.
    assert!(na.peer(&b.key.id()).is_none());
    assert!(na.encrypted_peer(&b.key.id()).is_some());
    assert!(handle
        .request_manifest(FOLDER, ContentHash([0; 32]))
        .await
        .is_err());

    eventually("b has a's items", || b.count() == 4).await;
    eventually("a has b's key", || a.count() == 4).await;
    // Pushed as they are made.
    a.add(a.version(2, Vec::new()));
    eventually("b has the new version", || b.count() == 5).await;
    assert_eq!(a.count(), 5, "nothing echoed back");
}

#[tokio::test(flavor = "multi_thread")]
async fn blobs_are_served_and_list_blobs_checked() {
    let user = UserKey::from_seed([2; 32]);
    let (a, b) = (
        Device::new(21, &user, &[&user]),
        Device::new(22, &user, &[&user]),
    );
    let list = vec![1u8, 2, 3];
    let list_id = BlobId(*blake3::hash(&list).as_bytes());
    let chunk_id = BlobId([8; 32]);
    let wrong_id = BlobId([9; 32]);
    a.blobs.lock().unwrap().insert(list_id, list.clone());
    a.blobs.lock().unwrap().insert(chunk_id, vec![4; 4112]);
    // A "list blob" that does not hash to its ID.
    a.blobs.lock().unwrap().insert(wrong_id, vec![1, 2]);
    let (na, nb) = (a.node().await, b.node().await);

    let got = nb
        .fetch_blobs(
            na.addr(),
            FOLDER,
            &[
                (chunk_id, BlobKind::SealedChunk),
                (list_id, BlobKind::List),
                (BlobId([0; 32]), BlobKind::SealedChunk),
            ],
        )
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![
            BlobResult::Found(chunk_id, vec![4; 4112]),
            BlobResult::Found(list_id, list),
            BlobResult::Missing(BlobId([0; 32])),
        ]
    );
    assert!(nb
        .fetch_blobs(na.addr(), FOLDER, &[(wrong_id, BlobKind::List)])
        .await
        .is_err());
    assert_eq!(
        nb.blob_availability(na.addr(), FOLDER, &[chunk_id, BlobId([0; 32]), list_id])
            .await
            .unwrap(),
        vec![true, false, true]
    );
    assert!(nb
        .is_full_encrypted_replica(na.addr(), FOLDER)
        .await
        .unwrap());

    // Not a member: nothing.
    let stranger = Device::new(23, &UserKey::from_seed([3; 32]), &[&user]);
    let ns = stranger.node().await;
    assert!(ns
        .fetch_blobs(na.addr(), FOLDER, &[(chunk_id, BlobKind::SealedChunk)])
        .await
        .is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn certificates_of_signers_are_fetched_before_their_items() {
    let user = UserKey::from_seed([4; 32]);
    let (a, b, c) = (
        Device::new(41, &user, &[&user]),
        Device::new(42, &user, &[&user]),
        Device::new(43, &user, &[&user]),
    );
    // a holds a version by c, and c's certificate; b knows neither.
    a.add(c.version(1, Vec::new()));
    a.learn_certificate(c.cert.clone());
    let (na, nb) = (a.node().await, b.node().await);
    nb.connect_encrypted(na.addr()).await.unwrap();
    eventually("b has c's version", || b.count() == 1).await;
    assert!(b.certificates.lock().unwrap().contains_key(&c.key.id()));
}

/// A storage peer that cannot vouch for an author (its operator's member list may differ) makes
/// a member hold that author's items, never give up on them (ADR 0023); a member's answer does.
#[tokio::test(flavor = "multi_thread")]
async fn a_storage_peers_cannot_vouch_counts_for_nothing() {
    let user = UserKey::from_seed([5; 32]);
    let operator = UserKey::from_seed([6; 32]);
    let author = Device::new(51, &user, &[&user]);
    let storage = Device::new(52, &operator, &[&user]);
    let member = Device::new(53, &user, &[&user]).with_storage_peer(storage.key.id());
    storage.add(author.version(1, Vec::new()));
    storage.refuses.store(true, Ordering::Relaxed);
    let (ns, nm) = (storage.node().await, member.node().await);

    nm.connect_encrypted(ns.addr()).await.unwrap();
    // Asked, refused, and held: not applied, not given up on, and the index not acknowledged.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(member.count(), 0);
    assert!(member.cannot_vouch.lock().unwrap().is_empty());
    assert_eq!(
        EncryptedBackend::peer_progress(member.as_ref(), &FOLDER, storage.key.id()),
        (0, 0)
    );

    // The same answer from a member counts.
    let other = Device::new(54, &user, &[&user]);
    other.add(author.version(2, Vec::new()));
    other.refuses.store(true, Ordering::Relaxed);
    let no = other.node().await;
    nm.connect_encrypted(no.addr()).await.unwrap();
    eventually("the member's answer counts", || {
        member
            .cannot_vouch
            .lock()
            .unwrap()
            .contains(&author.key.id())
    })
    .await;
}
