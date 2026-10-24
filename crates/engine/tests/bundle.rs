//! The diagnostics bundle (ADR 0030): what it holds, and that nothing it should not hold gets
//! in. Unix only: it makes a file unreadable with its mode.
#![cfg(unix)]

use std::fs;
use std::io::Read;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use shoal_engine::diagnostics::write_crash;
use shoal_engine::{
    zip_bundle, BundleOptions, Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode,
    Retention,
};
use shoal_identity::{DeviceKey, UserKey};
use shoal_proto::FolderId;

const FOLDER: FolderId = FolderId([44; 32]);

/// Unzips a bundle into (name, text) pairs.
fn unzip(bytes: &[u8]) -> Vec<(String, String)> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    (0..zip.len())
        .map(|i| {
            let mut file = zip.by_index(i).unwrap();
            let mut text = String::new();
            file.read_to_string(&mut text).unwrap();
            (file.name().unwrap().into_owned(), text)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn bundles_hold_no_names_ids_or_addresses() {
    let tmp = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let key = DeviceKey::from_seed([60; 32]);
    let data = tmp.path().join("CanaryData");
    let root = tmp.path().join("CanaryFolder");
    fs::create_dir_all(root.join("CanaryDir")).unwrap();
    fs::write(root.join("CanaryDir/CanarySecret.txt"), b"the contents").unwrap();
    fs::write(root.join("CanaryLocked.txt"), b"locked").unwrap();
    let engine = Engine::start(
        EngineConfig {
            data_dir: data.clone(),
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
                path: root.clone(),
                mode: ReplicaMode::Full,
                members: vec![user.id()],
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
                confirm_deletes: Default::default(),
                encryption: None,
            }],
            min_free_space: None,
        },
        key.clone(),
        user.certify(key.id(), "CanaryDevice", 0),
    )
    .await
    .unwrap();
    engine.scan(FOLDER).await.unwrap();
    // A problem with a name in it, where permissions stop this user.
    tokio::time::sleep(Duration::from_millis(20)).await;
    fs::write(root.join("CanaryLocked.txt"), b"changed").unwrap();
    fs::set_permissions(
        root.join("CanaryLocked.txt"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    engine.scan(FOLDER).await.unwrap();

    let device = engine.id().to_string();
    let folder = FOLDER.to_string();
    let user_id = user.id().to_string();
    let root_text = root.display().to_string();
    // Log lines as the daemon writes them, and a crash, full of names and addresses.
    fs::create_dir_all(data.join("logs")).unwrap();
    let lines = [
        format!(
            r#"{{"timestamp":"2026-09-28T02:23:10.123Z","level":"WARN","fields":{{"message":"update failed","folder":"{folder}","path":"CanaryDir/CanarySecret.txt","error":"i/o error on {root_text}/CanaryDir/CanarySecret.txt: permission denied"}},"target":"shoal_engine::materialize"}}"#
        ),
        format!(
            r#"{{"timestamp":"2026-09-28T02:23:11Z","level":"DEBUG","fields":{{"message":"dial failed","peer":"{device}","error":"failed to connect to 127.0.0.1:4433 as {user_id}"}},"target":"shoal_engine::engine"}}"#
        ),
        "a line that is not JSON, about CanaryDir/CanarySecret.txt".to_owned(),
        // A name the engine never recorded, with spaces, in an error.
        format!(
            r#"{{"timestamp":"2026-09-28T02:23:12Z","level":"WARN","fields":{{"message":"cannot open file","error":"{root_text}/Canary Taxes 2025/Canary Settlement.pdf: permission denied"}},"target":"shoal_engine::scan"}}"#
        ),
    ];
    fs::write(data.join("logs/shoal.log"), lines.join("\n")).unwrap();
    write_crash(
        &data.join("crashes"),
        "engine",
        &format!(
            "panicked writing {root_text}/CanaryDir/CanarySecret.txt and {root_text}/Canary Taxes 2025/Canary Settlement.pdf: denied"
        ),
        "at crates/engine/src/materialize.rs:12",
    )
    .unwrap();
    engine.set_diagnostics_config(serde_json::json!({
        "data_dir": data.display().to_string(),
        "network": {
            "relay": { "urls": ["https://CanaryUser:hunter2@relay.example.com/CanaryPath?token=abc"] },
            "peers": [{ "device": device, "addrs": ["192.168.7.20:4433"] }],
        },
        "folders": [{ "id": folder, "path": root_text, "mount": "/mnt/Canary Drive" }],
    }));

    let entries = engine.diagnostics(&BundleOptions::default()).unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    for expected in [
        "summary.json",
        "config.json",
        "status.json",
        "problems.json",
        "recent-problems.json",
        "doctor.json",
        "logs/shoal.log",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} missing from {names:?}"
        );
    }
    assert!(names.iter().any(|n| n.starts_with("crashes/crash-")));
    let unzipped = unzip(&zip_bundle(&entries).unwrap());
    assert_eq!(unzipped.len(), entries.len());
    for (name, text) in &unzipped {
        for leak in [
            "Canary",
            "hunter2",
            "the contents",
            "192.168.7.20",
            "127.0.0.1",
            &device,
            &folder,
            &user_id,
            "$argon2",
        ] {
            assert!(!text.contains(leak), "{name} holds {leak:?}:\n{text}");
            assert!(!name.contains(leak), "{name} is named after {leak:?}");
        }
    }
    let problems = &unzipped
        .iter()
        .find(|(n, _)| n == "problems.json")
        .unwrap()
        .1;
    let root_user = fs::read(root.join("CanaryLocked.txt")).is_ok();
    if !root_user {
        assert!(problems.contains("permission_denied"), "{problems}");
        assert!(problems.contains(".txt"), "extensions stay: {problems}");
    }
    let status = &unzipped.iter().find(|(n, _)| n == "status.json").unwrap().1;
    assert!(status.contains(&format!("{}…", &folder[..8])), "{status}");
    let log = &unzipped
        .iter()
        .find(|(n, _)| n == "logs/shoal.log")
        .unwrap()
        .1;
    assert!(
        log.contains("update failed") && log.contains("local-ipv4"),
        "{log}"
    );
    assert!(log.contains("02:23:10.123"), "timestamps stay: {log}");

    // With paths included, names stay; IDs and addresses still do not.
    let with_paths = engine
        .diagnostics(&BundleOptions {
            include_paths: true,
            app: None,
        })
        .unwrap();
    let all: String = with_paths.iter().map(|e| e.text.as_str()).collect();
    assert!(all.contains("CanaryDir/CanarySecret.txt"));
    assert!(!all.contains(&device) && !all.contains("127.0.0.1") && !all.contains("hunter2"));

    fs::set_permissions(
        root.join("CanaryLocked.txt"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    engine.shutdown().await;
}
