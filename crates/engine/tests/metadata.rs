//! Where folder metadata is kept (ADR 0034): its own directory, and a refusal to start without
//! it rather than starting every folder afresh.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use shoal_engine::{
    Engine, EngineConfig, Error, FolderConfig, RelayConfig, ReplicaMode, Retention,
};
use shoal_identity::{DeviceKey, UserKey};
use shoal_proto::FolderId;

const FOLDER: FolderId = FolderId([34; 32]);

fn config(dir: &Path, metadata_dir: Option<PathBuf>, user: &UserKey) -> EngineConfig {
    EngineConfig {
        data_dir: dir.join("data"),
        metadata_dir,
        relay: RelayConfig::Disabled,
        address_lookup: false,
        local_discovery: false,
        watch: false,
        limits: Default::default(),
        bind: Some(std::net::SocketAddr::from(([127, 0, 0, 1], 0))),
        peers: Vec::new(),
        folders: vec![FolderConfig {
            id: FOLDER,
            path: dir.join("folder"),
            mode: ReplicaMode::Full,
            members: vec![user.id()],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(3600),
            sync: Default::default(),
            confirm_deletes: Default::default(),
            encryption: None,
        }],
        min_free_space: None,
    }
}

async fn start(
    config: EngineConfig,
    key: &DeviceKey,
    user: &UserKey,
) -> shoal_engine::Result<Engine> {
    let certificate = user.certify(key.id(), "test", 0);
    Engine::start(config, key.clone(), certificate).await
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_lives_where_it_is_told_and_is_not_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let key = DeviceKey::from_seed([2; 32]);
    let fast = dir.path().join("ssd");
    let database = fast.join(format!("{FOLDER}.redb"));

    fs::create_dir_all(dir.path().join("folder")).unwrap();
    fs::write(dir.path().join("folder/a.txt"), b"kept").unwrap();
    let engine = start(config(dir.path(), Some(fast.clone()), &user), &key, &user)
        .await
        .unwrap();
    engine.scan(FOLDER).await.unwrap();
    assert_eq!(engine.metadata_dir(), fast);
    engine.shutdown().await;
    assert!(database.exists(), "the database is in metadata_dir");
    assert!(
        !dir.path()
            .join("data/metadata")
            .join(format!("{FOLDER}.redb"))
            .exists(),
        "and not in data_dir"
    );

    // Pointed somewhere else without moving it: refused, not started afresh.
    let elsewhere = dir.path().join("other");
    let refused = start(
        config(dir.path(), Some(elsewhere.clone()), &user),
        &key,
        &user,
    )
    .await;
    match refused {
        Err(Error::MetadataLeftBehind { from, to }) => {
            assert_eq!((from, to), (fast.clone(), elsewhere.clone()));
        }
        other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
    }

    // Moved there, it starts with everything.
    fs::rename(&fast, &elsewhere).unwrap();
    let engine = start(
        config(dir.path(), Some(elsewhere.clone()), &user),
        &key,
        &user,
    )
    .await
    .unwrap();
    let files = engine.files(FOLDER).unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].on_disk);
    engine.shutdown().await;
}
