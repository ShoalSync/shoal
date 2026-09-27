//! Local network discovery (ADR 0029): two engines with no relay and no address lookup find
//! each other by device ID alone, through mDNS.
//!
//! Multicast needs a network interface, which the Nix build sandbox does not have, so the flake
//! skips this test.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, PeerAddr, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([29; 32]);

async fn start(dir: &Path, key: &DeviceKey, user: &UserKey, peers: Vec<PeerAddr>) -> Engine {
    let config = EngineConfig {
        data_dir: dir.join("data"),
        relay: RelayConfig::Disabled,
        address_lookup: false,
        local_discovery: true,
        watch: false,
        limits: Default::default(),
        bind: Some(SocketAddr::from(([0, 0, 0, 0], 0))),
        peers,
        folders: vec![FolderConfig {
            id: FOLDER,
            path: dir.join("folder"),
            mode: ReplicaMode::Full,
            members: vec![user.id()],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(3600),
            sync: Default::default(),
        }],
    };
    let certificate = user.certify(key.id(), "test", 0);
    Engine::start(config, key.clone(), certificate)
        .await
        .unwrap()
}

fn root(dir: &Path) -> PathBuf {
    dir.join("folder")
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_on_one_network_find_each_other() {
    let dir = tempfile::tempdir().unwrap();
    let (a_dir, b_dir) = (dir.path().join("a"), dir.path().join("b"));
    // Random keys, so parallel runs on one network never see each other's devices.
    let user = UserKey::generate().unwrap();
    let (a_key, b_key) = (
        DeviceKey::generate().unwrap(),
        DeviceKey::generate().unwrap(),
    );

    fs::create_dir_all(root(&a_dir)).unwrap();
    fs::write(root(&a_dir).join("hello.txt"), b"found on the LAN").unwrap();
    let a = start(&a_dir, &a_key, &user, Vec::new()).await;
    a.scan(FOLDER).await.unwrap();

    // B knows only A's device ID: no addresses, no relay and no DNS lookup.
    let only_id = PeerAddr {
        device: a_key.id(),
        addrs: Vec::new(),
        relay: None,
    };
    let b = start(&b_dir, &b_key, &user, vec![only_id]).await;
    assert!(a.local_discovery() && b.local_discovery());

    for _ in 0..600 {
        if fs::read(root(&b_dir).join("hello.txt")).ok().as_deref()
            == Some(b"found on the LAN".as_slice())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("b never reached a through local discovery");
}
