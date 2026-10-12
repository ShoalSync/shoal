//! The `/v1` API over a real engine on a Unix socket, driven through the `sdk` client.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tidefell_engine::{Engine, EngineConfig, RelayConfig};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_sdk::types::{
    Direction, ErrorKind, Event, Folder, KnownPeer, Limits, LocalDeletes, NewFolder, SyncOptions,
};
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
                metadata_dir: None,
                relay: RelayConfig::Disabled,
                address_lookup: false,
                local_discovery: false,
                watch: false,
                limits: Default::default(),
                bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
                peers: Vec::new(),
                folders: Vec::new(),
                min_free_space: None,
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

    // Limits change until restart; below 64 KiB/s they are raised to it.
    assert_eq!(client.limits().await.unwrap(), Limits::default());
    let set = client
        .set_limits(&Limits {
            upload_kib_per_sec: Some(16),
            download_kib_per_sec: Some(4096),
            limit_local: true,
        })
        .await
        .unwrap();
    assert_eq!(set.upload_kib_per_sec, Some(64));
    assert_eq!(set.download_kib_per_sec, Some(4096));
    assert_eq!(client.limits().await.unwrap(), set);
    assert_eq!(engine.limits().upload, Some(64 * 1024));
    client.set_limits(&Limits::default()).await.unwrap();

    // Pausing the device is reported in status and survives in the engine until resumed.
    client.pause(None).await.unwrap();
    assert!(client.status().await.unwrap().paused);
    client.resume(None).await.unwrap();
    assert!(!client.status().await.unwrap().paused);
    assert_eq!(
        api_error(client.pause(Some(&"ab".repeat(32))).await),
        ErrorKind::NotFound
    );

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
            sync: Default::default(),
        })
        .await
        .unwrap();
    assert_eq!(created.members, vec![user.id().to_string()]);
    assert_eq!(created.retention_days, Some(7));
    assert!(!created.configured);
    // The state and last scan change as the first scan runs.
    let settled = |f: Folder| Folder {
        state: created.state,
        last_scan_ms: None,
        last_scan_duration_ms: None,
        ..f
    };
    let listed: Vec<Folder> = client
        .folders()
        .await
        .unwrap()
        .into_iter()
        .map(settled)
        .collect();
    assert_eq!(listed, vec![created.clone()]);
    assert_eq!(settled(client.folder(&created.id).await.unwrap()), created);
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
    // Made here, and no full replica holds it yet.
    assert!(files[0].only_here);
    assert_eq!(files[0].confirmed_copies, 0);
    assert_eq!(files[0].author, client.status().await.unwrap().device);
    assert!(!files[0].author_name.is_empty());
    assert_eq!(client.folder(&created.id).await.unwrap().only_here, 1);
    assert!(client
        .copies(&created.id, "a.txt")
        .await
        .unwrap()
        .is_empty());
    assert!(client.copies(&created.id, "missing.txt").await.is_err());
    // Deleted through the engine, which keeps the content for the recycle bin; a file deleted by
    // hand on the only device has no copy left to restore (ADR 0027).
    let folder_id: tidefell_proto::FolderId = created.id.parse().unwrap();
    engine.delete(folder_id, "a.txt").await.unwrap();
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

    // Removing a file from this device only, and sync options.
    client.remove_here(&created.id, "a.txt").await.unwrap();
    assert!(!root.join("a.txt").exists());
    let bin = client.recycle_bin(&created.id).await.unwrap();
    assert_eq!(bin.len(), 1);
    assert!(bin[0].here_only);
    assert_eq!(
        api_error(client.remove_here(&created.id, "a.txt").await),
        ErrorKind::NotFound
    );
    client.restore(&created.id, &bin[0].entry).await.unwrap();
    assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"hello api");

    // Renaming on every device, as offered for a name a drive cannot store (ADR 0033).
    client
        .rename(&created.id, "a.txt", "renamed/a.txt")
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(root.join("renamed/a.txt")).unwrap(),
        b"hello api"
    );
    assert_eq!(
        client.files(&created.id).await.unwrap()[0].path,
        "renamed/a.txt"
    );
    assert_eq!(
        api_error(client.rename(&created.id, "a.txt", "b.txt").await),
        ErrorKind::NotFound
    );
    assert_eq!(
        api_error(
            client
                .rename(&created.id, "renamed/a.txt", "../a.txt")
                .await
        ),
        ErrorKind::BadRequest
    );
    client
        .rename(&created.id, "renamed/a.txt", "a.txt")
        .await
        .unwrap();

    let backup_only = SyncOptions {
        direction: Direction::SendOnly,
        local_deletes: LocalDeletes::ThisDevice,
    };
    let changed = client.set_sync(&created.id, backup_only).await.unwrap();
    assert_eq!(changed.sync, backup_only);
    let changed = client
        .set_sync(&created.id, SyncOptions::default())
        .await
        .unwrap();
    assert_eq!(changed.sync, SyncOptions::default());

    // Retention and emptying the recycle bin.
    let forever = client.set_retention(&created.id, None).await.unwrap();
    assert_eq!(forever.retention_days, None);
    for name in ["b.txt", "c.txt"] {
        std::fs::write(root.join(name), b"to delete").unwrap();
    }
    client.scan(&created.id).await.unwrap();
    for name in ["b.txt", "c.txt"] {
        std::fs::remove_file(root.join(name)).unwrap();
    }
    client.scan(&created.id).await.unwrap();
    let folder = client.set_retention(&created.id, Some(3)).await.unwrap();
    assert_eq!(folder.retention_days, Some(3));
    assert_eq!(folder.recycle_bin.items, 2);
    assert_eq!(folder.recycle_bin.bytes, 18);
    let bin = client.recycle_bin(&created.id).await.unwrap();
    assert_eq!(
        folder.recycle_bin.next_expiry_ms,
        bin.iter().filter_map(|r| r.expires_at_ms).min()
    );
    let discarded = client.discard(&created.id, &bin[0].entry).await.unwrap();
    assert_eq!(discarded.entry, bin[0].entry);
    assert_eq!(
        api_error(client.discard(&created.id, &bin[0].entry).await),
        ErrorKind::NotFound
    );
    let emptied = client.empty_recycle_bin(&created.id).await.unwrap();
    assert_eq!(emptied.len(), 1);
    assert_eq!(emptied[0].entry, bin[1].entry);
    let folder = client.folder(&created.id).await.unwrap();
    assert_eq!(folder.recycle_bin, Default::default());

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
        sync: Default::default(),
    };
    assert_eq!(
        api_error(client.add_folder(&duplicate).await),
        ErrorKind::Conflict
    );

    // Peers.
    let peer = KnownPeer {
        device: DeviceKey::from_seed([9; 32]).id().to_string(),
        addrs: vec!["127.0.0.1:9".into()],
        relay: Some("https://relay.example.org./".into()),
    };
    client.add_peer(&peer).await.unwrap();
    assert_eq!(client.known_peers().await.unwrap(), vec![peer.clone()]);
    client.remove_peer(&peer.device).await.unwrap();
    assert_eq!(
        api_error(client.remove_peer(&peer.device).await),
        ErrorKind::NotFound
    );
    assert!(client.peers().await.unwrap().is_empty());

    // Status and problems (ADR 0030): no peers, nothing moved, nothing wrong.
    let status = client.status().await.unwrap();
    assert!(status.peers.is_empty());
    assert_eq!(status.transfer.sent, 0);
    assert!(client
        .folder_problems(&created.id)
        .await
        .unwrap()
        .is_empty());
    // A diagnostics bundle is a zip file; its summary names no full ID (ADR 0030).
    let zip = client.diagnostics(false).await.unwrap();
    assert!(zip.starts_with(b"PK"), "a zip file");
    let status = client.status().await.unwrap();
    assert!(!zip.windows(64).any(|w| w == status.device.as_bytes()));
    let checks = client.doctor().await.unwrap();
    assert!(checks
        .iter()
        .any(|c| c.name == "folder" && c.level == tidefell_sdk::types::CheckLevel::Pass));
    let folder = client
        .folders()
        .await
        .unwrap()
        .into_iter()
        .find(|f| f.id == created.id)
        .unwrap();
    assert_eq!(folder.problems, 0);
    assert_eq!(folder.pending_files, 0);
    assert!(folder.last_scan_ms.is_some());
    assert_eq!(
        api_error(client.folder_problems(&"ab".repeat(32)).await),
        ErrorKind::NotFound
    );
    // A crash written up for the tracker (ADR 0030): shown to copy, or linked when configured.
    tidefell_engine::diagnostics::write_crash(
        &engine.data_dir().join("crashes"),
        "rust",
        "boom",
        "location: src/x.rs:1",
    )
    .unwrap();
    let crash = client.crashes().await.unwrap().pop().unwrap();
    let report = client.crash_report(&crash.id).await.unwrap();
    assert_eq!(report.title, "Crash: boom");
    assert!(report.body.contains("location: src/x.rs:1"));
    assert_eq!(report.url, None);
    engine.set_report_url(Some("https://example.org/issues".into()));
    let report = client.crash_report(&crash.id).await.unwrap();
    assert!(report
        .url
        .unwrap()
        .starts_with("https://example.org/issues/new?title=Crash%3A%20boom&body="));
    for id in ["../device.key", "crash-0-none.txt", "seen"] {
        assert_eq!(
            api_error(client.crash_report(id).await),
            ErrorKind::NotFound,
            "{id}"
        );
    }
    client.dismiss_crashes().await.unwrap();

    // The activity log (ADR 0030): everything above, newest first, all made here.
    use tidefell_sdk::types::ChangeKind;
    let activity = client.activity(&created.id, 100).await.unwrap();
    let oldest = activity.last().unwrap();
    assert_eq!(
        (oldest.kind, oldest.path.as_str()),
        (ChangeKind::Added, "a.txt")
    );
    assert_eq!(activity[0].kind, ChangeKind::Deleted);
    assert!(activity
        .iter()
        .any(|a| (a.kind, a.path.as_str()) == (ChangeKind::Restored, "a.txt")));
    assert!(activity
        .iter()
        .all(|a| a.local && a.device == status.device));
    assert!(activity.windows(2).all(|w| w[0].time_ms >= w[1].time_ms));
    assert_eq!(client.activity(&created.id, 2).await.unwrap().len(), 2);
    assert_eq!(
        api_error(client.activity(&"ab".repeat(32), 10).await),
        ErrorKind::NotFound
    );

    // Remove the folder.
    client.remove_folder(&created.id).await.unwrap();
    assert!(client.folders().await.unwrap().is_empty());
    assert!(root.join("a.txt").exists(), "files stay on disk");

    // The OpenAPI document describes every route.
    let doc = client.openapi().await.unwrap();
    for path in [
        "/v1/status",
        "/v1/folders",
        "/v1/folders/{id}/retention",
        "/v1/folders/{id}/copies",
        "/v1/folders/{id}/recycle/{entry}",
        "/v1/folders/{id}/recycle/{entry}/restore",
        "/v1/events",
        "/v1/log",
        "/v1/log/problems",
        "/v1/crashes",
        "/v1/crashes/dismiss",
        "/v1/crashes/report",
        "/v1/limits",
        "/v1/pause",
        "/v1/folders/{id}/pause",
        "/v1/folders/{id}/problems",
        "/v1/folders/{id}/activity",
        "/v1/doctor",
        "/v1/diagnostics",
        "/v1/folders/{id}/resolve",
        "/v1/folders/{id}/versions/open",
    ] {
        assert!(doc["paths"][path].is_object(), "OpenAPI is missing {path}");
    }
    assert!(doc["components"]["schemas"]["NewFolder"].is_object());

    events.abort();
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn logging_through_the_api() {
    let tmp = tempfile::tempdir().unwrap();
    let key = DeviceKey::from_seed([3; 32]);
    let data_dir = tmp.path().join("data");
    let engine = Arc::new(
        Engine::start(
            EngineConfig {
                data_dir: data_dir.clone(),
                metadata_dir: None,
                relay: RelayConfig::Disabled,
                address_lookup: false,
                local_discovery: false,
                watch: false,
                limits: Default::default(),
                bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
                peers: Vec::new(),
                folders: Vec::new(),
                min_free_space: None,
            },
            key.clone(),
            UserKey::from_seed([1; 32]).certify(key.id(), "log-test", 0),
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

    // Before the process starts logging there is nothing to show.
    assert_eq!(api_error(client.log_settings().await), ErrorKind::NotFound);
    tidefell_engine::diagnostics::init_logging(&tidefell_engine::diagnostics::LogOptions {
        data_dir: data_dir.clone(),
        level: "info".into(),
        file: Some(true),
        stderr: false,
    })
    .unwrap();

    let settings = client.log_settings().await.unwrap();
    assert_eq!(settings.level, "info");
    assert_eq!(
        settings.file.as_deref(),
        Some(data_dir.join("logs/tidefell.log").to_str().unwrap())
    );
    assert_eq!(
        client
            .set_log_level("debug,tidefell_net=trace")
            .await
            .unwrap()
            .level,
        "debug,tidefell_net=trace"
    );
    assert_eq!(
        api_error(client.set_log_level("[nope").await),
        ErrorKind::BadRequest
    );
    assert_eq!(
        client.log_settings().await.unwrap().level,
        "debug,tidefell_net=trace"
    );

    tracing::warn!(folder = "f1", "a warning for the problems list");
    let problems = client.problems().await.unwrap();
    let problem = problems.last().unwrap();
    assert_eq!(problem.message, "a warning for the problems list");
    assert_eq!(problem.fields["folder"], "f1");

    // A crash from an earlier run is reported until it is dismissed.
    assert!(client.status().await.unwrap().crash.is_none());
    tidefell_engine::diagnostics::write_crash(&data_dir.join("crashes"), "test", "boom", "")
        .unwrap();
    let crash = client.status().await.unwrap().crash.unwrap();
    assert_eq!(crash.message, "boom");
    assert!(!crash.seen);
    assert_eq!(client.crashes().await.unwrap(), vec![crash]);
    client.dismiss_crashes().await.unwrap();
    assert!(client.status().await.unwrap().crash.is_none());
    assert!(client.crashes().await.unwrap()[0].seen);

    server.abort();
}
