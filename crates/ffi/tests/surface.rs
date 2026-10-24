//! The ffi surface as the Android app uses it: pair, start a node, join an offered folder as a
//! partial replica, open a placeholder, save a file and receive events.

use std::fs;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use shoal_engine::pairing::parse_code;
use shoal_engine::{
    DeviceKey, Engine, EngineConfig, FolderConfig, FolderId, RelayConfig, ReplicaMode, Retention,
    UserKey,
};
use shoal_ffi::{
    device_id, is_paired, EventListener, Network, Node, NodeEvent, Pairing, PairingUpdate, Relay,
};

const FOLDER: FolderId = FolderId([33; 32]);

fn network() -> Network {
    Network {
        relay: Relay::Disabled,
        address_lookup: false,
        local_discovery: false,
        bind: Some("127.0.0.1:0".into()),
    }
}

struct Events(Arc<Mutex<Vec<NodeEvent>>>);

impl EventListener for Events {
    fn on_event(&self, event: NodeEvent) {
        self.0.lock().unwrap().push(event);
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
async fn pair_join_open_and_save() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let laptop_key = DeviceKey::from_seed([70; 32]);
    let laptop_root = tmp.path().join("laptop/Photos");
    fs::create_dir_all(&laptop_root).unwrap();
    fs::write(laptop_root.join("beach.jpg"), b"sand and sea").unwrap();
    let laptop = Arc::new(
        Engine::start(
            EngineConfig {
                data_dir: tmp.path().join("laptop/data"),
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
                    path: laptop_root.clone(),
                    mode: ReplicaMode::Full,
                    members: vec![alice.id()],
                    retention: Retention::Days(30),
                    scan_interval: Duration::from_secs(1),
                    sync: Default::default(),
                    confirm_deletes: Default::default(),
                    encryption: None,
                }],
                min_free_space: None,
            },
            laptop_key.clone(),
            alice.certify(laptop_key.id(), "laptop", 0),
        )
        .await
        .unwrap(),
    );
    laptop.scan(FOLDER).await.unwrap();

    // The phone.
    let data = tmp.path().join("phone").display().to_string();
    let id = device_id(data.clone()).unwrap();
    assert_eq!(id, device_id(data.clone()).unwrap(), "the key is kept");
    assert!(!is_paired(data.clone()).unwrap());
    assert!(matches!(
        Node::start(data.clone(), network()).await,
        Err(shoal_ffi::ShoalError::NotPaired)
    ));

    let pairing = Pairing::start(data.clone(), network(), "Pixel".into())
        .await
        .unwrap();
    let code = parse_code(&pairing.code()).unwrap();
    let approve = {
        let (laptop, alice) = (laptop.clone(), alice.clone());
        tokio::spawn(async move {
            let pending = laptop.pair(&code).await.unwrap();
            pending.approve(&alice, "Alice's phone").await.unwrap();
        })
    };
    let PairingUpdate::Confirming {
        approver_name,
        confirmation_code,
        ..
    } = pairing.next_update().await.unwrap()
    else {
        panic!("expected confirming");
    };
    assert_eq!(approver_name, "laptop");
    assert_eq!(confirmation_code.len(), 6);
    let PairingUpdate::Paired {
        name,
        user,
        folders,
    } = pairing.next_update().await.unwrap()
    else {
        panic!("expected paired");
    };
    approve.await.unwrap();
    assert_eq!(name, "Alice's phone");
    assert_eq!(user, alice.id().to_string());
    assert_eq!(folders.len(), 1);
    assert!(matches!(
        pairing.next_update().await.unwrap(),
        PairingUpdate::Stopped
    ));
    assert!(is_paired(data.clone()).unwrap());

    // The node joins the offered folder as a partial replica.
    let node = Node::start(data.clone(), network()).await.unwrap();
    assert_eq!(node.status().unwrap().device, id);
    let events = Arc::new(Mutex::new(Vec::new()));
    node.subscribe(Box::new(Events(events.clone()))).unwrap();
    let offer = &folders[0];
    let root = tmp.path().join("phone-files/Photos");
    // Called from a plain thread, as from Kotlin: no Tokio runtime is current.
    let folder = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                node.join_folder(
                    offer.id.clone(),
                    root.display().to_string(),
                    Some(10_000_000),
                    offer.members.clone(),
                    offer.retention_days,
                    shoal_ffi::SyncOptions {
                        direction: shoal_ffi::Direction::Both,
                        local_deletes: shoal_ffi::LocalDeletes::Everywhere,
                    },
                    offer.encryption.clone(),
                )
            })
            .join()
            .unwrap()
    })
    .unwrap();
    assert_eq!(folder.partial_budget, Some(10_000_000));
    assert_eq!(node.folders().unwrap().len(), 1);

    eventually("the phone lists the photo", || {
        node.files(offer.id.clone()).unwrap().len() == 1
    })
    .await;
    let file = node.files(offer.id.clone()).unwrap().remove(0);
    assert_eq!(file.path, "beach.jpg");
    assert!(!file.on_disk);
    let local = node
        .open(offer.id.clone(), "beach.jpg".into())
        .await
        .unwrap();
    assert_eq!(fs::read(&local).unwrap(), b"sand and sea");
    node.close(offer.id.clone(), "beach.jpg".into()).unwrap();
    assert!(matches!(
        node.open(offer.id.clone(), "nope.jpg".into()).await,
        Err(shoal_ffi::ShoalError::NotFound { .. })
    ));

    // Saving a file on the phone reaches the laptop.
    let source = tmp.path().join("camera.tmp");
    fs::write(&source, b"new photo").unwrap();
    node.write(
        offer.id.clone(),
        "camera/new.jpg".into(),
        source.display().to_string(),
    )
    .await
    .unwrap();
    eventually("the laptop has the phone's photo", || {
        fs::read(laptop_root.join("camera/new.jpg")).ok().as_deref()
            == Some(b"new photo".as_slice())
    })
    .await;
    // The laptop is a full replica, so it confirms the copy (ADR 0022).
    let photo = || {
        node.files(offer.id.clone())
            .unwrap()
            .into_iter()
            .find(|f| f.path == "camera/new.jpg")
            .unwrap()
    };
    for _ in 0..150 {
        if photo().confirmed_copies == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(photo().confirmed_copies, 1);
    assert!(!photo().only_here);
    let copies = node
        .copies(offer.id.clone(), "camera/new.jpg".into())
        .unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].device, laptop_key.id().to_string());
    assert!(matches!(
        node.copies(offer.id.clone(), "nope.jpg".into()),
        Err(shoal_ffi::ShoalError::NotFound { .. })
    ));
    // Storage priorities are for encrypted folders, which this one is not.
    assert!(matches!(
        node.priorities(offer.id.clone()).await,
        Err(shoal_ffi::ShoalError::Invalid { .. })
    ));
    assert!(matches!(
        node.storage_report(offer.id.clone()).await,
        Err(shoal_ffi::ShoalError::Invalid { .. })
    ));
    // Deleting sends it to the recycle bin, which this device can keep longer or empty.
    node.delete(offer.id.clone(), "camera/new.jpg".into())
        .await
        .unwrap();
    eventually("the laptop removes the deleted photo", || {
        !laptop_root.join("camera/new.jpg").exists()
    })
    .await;
    let bin = node.recycle_bin(offer.id.clone()).unwrap();
    assert_eq!(bin.len(), 1);
    assert_eq!(bin[0].path, "camera/new.jpg");
    let folder = node.set_retention(offer.id.clone(), 0).unwrap();
    assert_eq!(folder.retention_days, 0);
    assert_eq!(folder.recycled_files, 1);
    assert_eq!(folder.recycled_bytes, 9);
    assert_eq!(folder.next_purge_ms, None);
    let folder = node.set_retention(offer.id.clone(), 7).unwrap();
    assert_eq!(
        folder.next_purge_ms,
        Some(bin[0].deleted_at_ms + 7 * 86_400_000)
    );
    let discarded = node
        .discard(offer.id.clone(), bin[0].entry.clone())
        .unwrap();
    assert_eq!(discarded.path, "camera/new.jpg");
    assert!(matches!(
        node.discard(offer.id.clone(), bin[0].entry.clone()),
        Err(shoal_ffi::ShoalError::NotFound { .. })
    ));
    assert!(node.empty_recycle_bin(offer.id.clone()).unwrap().is_empty());
    assert_eq!(node.folders().unwrap()[0].recycled_files, 0);

    eventually("events arrive", || {
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, NodeEvent::FileUpdated { path, .. } if path == "beach.jpg"))
    })
    .await;

    node.shutdown().await.unwrap();
    assert!(node.folders().is_err(), "a stopped node refuses calls");
}
