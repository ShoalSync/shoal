//! The `/v1` API over a real engine on a Unix socket, driven through the `sdk` client.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tidefell_engine::{Engine, EngineConfig, RelayConfig};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_sdk::types::{ErrorKind, Event, KnownPeer, NewFolder};
use tidefell_sdk::{Client, Error};

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    for _ in 0..500 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn api_error(result: Result<impl std::fmt::Debug, Error>) -> ErrorKind {
    match result {
        Err(Error::Api(e)) => e.kind,
        other => panic!("expected an API error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn control_api_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let key = DeviceKey::from_seed([2; 32]);
    let data_dir = tmp.path().join("data");
    let engine = Arc::new(
        Engine::start(
            EngineConfig {
                data_dir: data_dir.clone(),
                relay: RelayConfig::Disabled,
                address_lookup: false,
                bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
                peers: Vec::new(),
                folders: Vec::new(),
            },
            key.clone(),
            user.certify(key.id(), "api-test", 0),
        )
        .await
        .unwrap(),
    );
    let socket = Client::socket_path(&data_dir);
    let server = tokio::spawn({
        let (engine, socket) = (engine.clone(), socket.clone());
        async move { tidefell_api::serve_unix(engine, &socket).await }
    });
    let client = Client::unix(&socket);
    eventually("socket is up", async || client.status().await.is_ok()).await;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "control socket is private to the daemon user"
        );
    }

    let status = client.status().await.unwrap();
    assert_eq!(status.device, key.id().to_string());
    assert_eq!(status.user, user.id().to_string());
    assert_eq!(status.name, "api-test");

    // Events: subscribe before changing anything.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let events = tokio::spawn({
        let client = client.clone();
        async move {
            client.events(move |e| tx.send(e).is_ok()).await.unwrap();
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a folder; members default to this device's user.
    let root = tmp.path().join("folder");
    let created = client
        .add_folder(&NewFolder {
            id: None,
            path: root.display().to_string(),
            members: Vec::new(),
            retention_days: 7,
            mode: None,
            budget_bytes: None,
            scan_interval_secs: Some(3600),
        })
        .await
        .unwrap();
    assert_eq!(created.members, vec![user.id().to_string()]);
    assert_eq!(created.retention_days, Some(7));
    assert!(!created.configured);
    assert_eq!(client.folders().await.unwrap(), vec![created.clone()]);
    assert_eq!(client.folder(&created.id).await.unwrap(), created);
    let added = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        added,
        Event::FolderAdded {
            folder: created.id.clone()
        }
    );

    // Files, delete, recycle bin and restore.
    std::fs::write(root.join("a.txt"), b"hello api").unwrap();
    client.scan(&created.id).await.unwrap();
    let files = client.files(&created.id).await.unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!((files[0].path.as_str(), files[0].size), ("a.txt", 9));
    std::fs::remove_file(root.join("a.txt")).unwrap();
    client.scan(&created.id).await.unwrap();
    assert!(client.files(&created.id).await.unwrap().is_empty());
    let bin = client.recycle_bin(&created.id).await.unwrap();
    assert_eq!(bin.len(), 1);
    assert_eq!(bin[0].path, "a.txt");
    assert!(bin[0].expires_at_ms.is_some());
    client.restore(&created.id, &bin[0].entry).await.unwrap();
    eventually("restored file on disk", async || {
        std::fs::read(root.join("a.txt")).ok().as_deref() == Some(b"hello api".as_slice())
    })
    .await;
    assert!(client.recycle_bin(&created.id).await.unwrap().is_empty());
    assert_eq!(
        api_error(client.restore(&created.id, &bin[0].entry).await),
        ErrorKind::NotFound
    );

    // Members.
    let bob = UserKey::from_seed([3; 32]).id().to_string();
    let with_bob = client.add_member(&created.id, &bob).await.unwrap();
    assert!(with_bob.members.contains(&bob));
    let without_bob = client.remove_member(&created.id, &bob).await.unwrap();
    assert!(!without_bob.members.contains(&bob));
    assert_eq!(
        api_error(
            client
                .remove_member(&created.id, &user.id().to_string())
                .await
        ),
        ErrorKind::BadRequest,
        "the last member cannot be removed"
    );

    // Errors.
    let missing = "f".repeat(64);
    assert_eq!(
        api_error(client.folder(&missing).await),
        ErrorKind::NotFound
    );
    assert_eq!(
        api_error(client.folder("not-hex").await),
        ErrorKind::BadRequest
    );
    let duplicate = NewFolder {
        id: Some(created.id.clone()),
        path: root.display().to_string(),
        members: Vec::new(),
        retention_days: 0,
        mode: None,
        budget_bytes: None,
        scan_interval_secs: None,
    };
    assert_eq!(
        api_error(client.add_folder(&duplicate).await),
        ErrorKind::Conflict
    );

    // Peers.
    let peer = KnownPeer {
        device: DeviceKey::from_seed([9; 32]).id().to_string(),
        addrs: vec!["127.0.0.1:9".into()],
    };
    client.add_peer(&peer).await.unwrap();
    assert_eq!(client.known_peers().await.unwrap(), vec![peer.clone()]);
    client.remove_peer(&peer.device).await.unwrap();
    assert_eq!(
        api_error(client.remove_peer(&peer.device).await),
        ErrorKind::NotFound
    );
    assert!(client.peers().await.unwrap().is_empty());

    // Remove the folder.
    client.remove_folder(&created.id).await.unwrap();
    assert!(client.folders().await.unwrap().is_empty());
    assert!(root.join("a.txt").exists(), "files stay on disk");

    // The OpenAPI document describes every route.
    let doc = client.openapi().await.unwrap();
    for path in [
        "/v1/status",
        "/v1/folders",
        "/v1/folders/{id}/recycle/{entry}/restore",
        "/v1/events",
    ] {
        assert!(doc["paths"][path].is_object(), "OpenAPI is missing {path}");
    }
    assert!(doc["components"]["schemas"]["NewFolder"].is_object());

    events.abort();
    server.abort();
}
