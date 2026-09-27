//! A new device pairs with a running engine (ADR 0015), then joins an offered folder as a
//! partial replica and opens a file.

use std::fs;
use std::net::SocketAddr;
use std::time::Duration;

use tidefell_engine::pairing::{self, PairingEvent};
use tidefell_engine::{Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode, Retention};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([12; 32]);

fn localhost() -> Option<SocketAddr> {
    Some(SocketAddr::from(([127, 0, 0, 1], 0)))
}

async fn next(server: &mut pairing::PairingServer) -> PairingEvent {
    tokio::time::timeout(Duration::from_secs(10), server.next_event())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_device_pairs_then_syncs() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);

    // The laptop has the user key and a folder with a file.
    let laptop_key = DeviceKey::from_seed([60; 32]);
    let laptop_root = tmp.path().join("laptop/Photos");
    fs::create_dir_all(&laptop_root).unwrap();
    fs::write(laptop_root.join("beach.jpg"), b"sand and sea").unwrap();
    let laptop = Engine::start(
        EngineConfig {
            data_dir: tmp.path().join("laptop/data"),
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            bind: localhost(),
            peers: Vec::new(),
            folders: vec![FolderConfig {
                id: FOLDER,
                path: laptop_root.clone(),
                mode: ReplicaMode::Full,
                members: vec![alice.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
            }],
        },
        laptop_key.clone(),
        alice.certify(laptop_key.id(), "laptop", 0),
    )
    .await
    .unwrap();
    laptop.scan(FOLDER).await.unwrap();

    // The phone shows a code; the laptop scans it and the person confirms.
    let phone_key = DeviceKey::from_seed([61; 32]);
    let mut server = pairing::start_pairing(
        &phone_key,
        RelayConfig::Disabled,
        false,
        false,
        localhost(),
        "Pixel",
    )
    .await
    .unwrap();
    let text = server.code().to_text();
    let code = pairing::parse_code(&text).unwrap();

    let pending = laptop.pair(&code).await.unwrap();
    assert_eq!(pending.suggested_name(), "Pixel");
    let PairingEvent::Confirming {
        confirmation_code,
        approver,
    } = next(&mut server).await
    else {
        panic!("expected confirming");
    };
    assert_eq!(confirmation_code, pending.confirmation_code());
    assert_eq!(approver.name, "laptop");

    // Only this device's user can approve.
    let mallory = UserKey::from_seed([9; 32]);
    let pending_err = pending.approve(&mallory, "phone").await;
    assert!(pending_err.is_err());
    // That attempt was dropped; the code is still valid for a new one.
    let _failed = next(&mut server).await;
    let pending = laptop.pair(&code).await.unwrap();
    let _confirming = next(&mut server).await;
    pending.approve(&alice, "Alice's phone").await.unwrap();

    let PairingEvent::Paired(paired) = next(&mut server).await else {
        panic!("expected paired");
    };
    server.shutdown().await;
    assert_eq!(paired.folders.len(), 1);
    assert_eq!(paired.folders[0].id, FOLDER);
    assert_eq!(paired.folders[0].name, "Photos");
    assert_eq!(paired.folders[0].retention_days, 30);

    // The phone starts as a normal peer, keeps the laptop connected and joins the folder.
    let phone_data = tmp.path().join("phone/data");
    pairing::remember_approver(&phone_data, &paired).unwrap();
    let phone = Engine::start(
        EngineConfig {
            data_dir: phone_data,
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            bind: localhost(),
            peers: Vec::new(),
            folders: Vec::new(),
        },
        phone_key.clone(),
        paired.certificate.clone(),
    )
    .await
    .unwrap();
    assert_eq!(phone.known_peers()[0].device, laptop_key.id());
    let offer = &paired.folders[0];
    phone
        .add_folder(FolderConfig {
            id: offer.id,
            path: tmp.path().join("phone/Photos"),
            mode: ReplicaMode::Partial {
                budget_bytes: 1 << 20,
            },
            members: offer.members.clone(),
            retention: Retention::Days(offer.retention_days),
            scan_interval: Duration::from_secs(3600),
            sync: Default::default(),
        })
        .unwrap();

    let mut listed = false;
    for _ in 0..500 {
        if phone.files(FOLDER).unwrap().len() == 1 {
            listed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(listed, "the phone lists the laptop's file");
    let local = phone.open(FOLDER, "beach.jpg").await.unwrap();
    assert_eq!(fs::read(local).unwrap(), b"sand and sea");
    phone.close(FOLDER, "beach.jpg").unwrap();

    phone.shutdown().await;
    laptop.shutdown().await;
}
