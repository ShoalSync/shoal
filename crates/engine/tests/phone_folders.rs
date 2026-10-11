//! Phone folders (ADR 0025): send-only replicas, deletes that stay on this device, and files
//! removed from this device into its recycle bin.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Direction, Engine, EngineConfig, Error, FolderConfig, LocalDeletes, RelayConfig, ReplicaMode,
    Retention, SyncOptions,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([25; 32]);

const TWO_WAY: SyncOptions = SyncOptions {
    direction: Direction::Both,
    local_deletes: LocalDeletes::ThisDevice,
};

const BACKUP_ONLY: SyncOptions = SyncOptions {
    direction: Direction::SendOnly,
    local_deletes: LocalDeletes::ThisDevice,
};

struct Device {
    root: PathBuf,
    data: PathBuf,
    key: DeviceKey,
    user: UserKey,
}

impl Device {
    fn new(dir: &Path, name: &str, seed: u8, user: &UserKey) -> Self {
        Self {
            root: dir.join(name).join("folder"),
            data: dir.join(name).join("data"),
            key: DeviceKey::from_seed([seed; 32]),
            user: user.clone(),
        }
    }

    fn folder(&self, sync: SyncOptions) -> FolderConfig {
        FolderConfig {
            id: FOLDER,
            path: self.root.clone(),
            mode: ReplicaMode::Full,
            members: vec![self.user.id()],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(3600),
            sync,
        }
    }

    async fn start(&self) -> Engine {
        let config = EngineConfig {
            data_dir: self.data.clone(),
            metadata_dir: None,
            relay: RelayConfig::Disabled,
            address_lookup: false,
            local_discovery: false,
            watch: false,
            limits: Default::default(),
            bind: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
            peers: Vec::new(),
            folders: Vec::new(),
        };
        let certificate = self.user.certify(self.key.id(), "test", 0);
        Engine::start(config, self.key.clone(), certificate)
            .await
            .unwrap()
    }

    fn write(&self, path: &str, data: &[u8]) {
        let abs = self.root.join(path);
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(abs, data).unwrap();
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.root.join(path)).ok()
    }

    fn set_aside_files(&self) -> usize {
        fs::read_dir(self.root.join(".tidefell").join("recycle"))
            .map(|d| d.count())
            .unwrap_or(0)
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..1500 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn on_disk(engine: &Engine, path: &str) -> bool {
    engine
        .files(FOLDER)
        .unwrap()
        .iter()
        .any(|f| f.path == path && f.on_disk)
}

fn listed(engine: &Engine, path: &str) -> bool {
    engine.files(FOLDER).unwrap().iter().any(|f| f.path == path)
}

/// Waits long enough for a write that should not happen to have happened.
async fn settle(engine: &Engine, path: &str) {
    eventually("the change is listed", || listed(engine, path)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_way_deletes_stay_on_this_device() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([3; 32]);
    let phone = Device::new(tmp.path(), "phone", 40, &alice);
    let nas = Device::new(tmp.path(), "nas", 41, &alice);
    nas.write("trip.jpg", b"beach");
    let engine_nas = nas.start().await;
    engine_nas
        .add_folder(nas.folder(SyncOptions::default()))
        .unwrap();
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_phone = phone.start().await;
    engine_phone.add_folder(phone.folder(TWO_WAY)).unwrap();
    engine_phone.connect(engine_nas.addr()).await.unwrap();

    eventually("the photo reaches the phone", || {
        phone.read("trip.jpg").as_deref() == Some(b"beach".as_slice())
    })
    .await;

    // Deleting it on the phone keeps it on the NAS and leaves nothing pending on the phone.
    fs::remove_file(phone.root.join("trip.jpg")).unwrap();
    engine_phone.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(nas.read("trip.jpg").as_deref(), Some(b"beach".as_slice()));
    assert!(engine_nas.recycle_bin(FOLDER).unwrap().is_empty());
    assert!(listed(&engine_phone, "trip.jpg"), "still listed");
    assert!(!on_disk(&engine_phone, "trip.jpg"));
    assert_eq!(engine_phone.status(FOLDER).unwrap().pending, 0);
    assert!(
        engine_phone.recycle_bin(FOLDER).unwrap().is_empty(),
        "a delete outside the app is not recycled"
    );

    // A new version of the dropped file stays off the phone; new files still arrive.
    nas.write("trip.jpg", b"beach, cropped");
    nas.write("new.jpg", b"sunset");
    engine_nas.scan(FOLDER).await.unwrap();
    eventually("the new photo reaches the phone", || {
        phone.read("new.jpg").is_some()
    })
    .await;
    settle(&engine_phone, "trip.jpg").await;
    assert!(
        phone.read("trip.jpg").is_none(),
        "dropped file not downloaded"
    );

    // Opening it brings it back, and later versions follow again.
    let opened = engine_phone.open(FOLDER, "trip.jpg").await.unwrap();
    engine_phone.close(FOLDER, "trip.jpg").unwrap();
    assert_eq!(fs::read(opened).unwrap(), b"beach, cropped");
    nas.write("trip.jpg", b"beach, final");
    engine_nas.scan(FOLDER).await.unwrap();
    eventually("the next version reaches the phone", || {
        phone.read("trip.jpg").as_deref() == Some(b"beach, final".as_slice())
    })
    .await;

    // A delete on the NAS still reaches the phone's recycle bin.
    fs::remove_file(nas.root.join("new.jpg")).unwrap();
    engine_nas.scan(FOLDER).await.unwrap();
    eventually("the delete reaches the phone", || {
        phone.read("new.jpg").is_none()
    })
    .await;
    let bin = engine_phone.recycle_bin(FOLDER).unwrap();
    assert_eq!(bin.len(), 1);
    assert!(!bin[0].here_only);

    engine_phone.shutdown().await;
    engine_nas.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn send_only_folders_never_write_other_devices_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([4; 32]);
    let phone = Device::new(tmp.path(), "phone", 42, &alice);
    let nas = Device::new(tmp.path(), "nas", 43, &alice);
    phone.write("DCIM/one.jpg", b"from the phone");
    nas.write("desktop.txt", b"from the NAS");
    let engine_nas = nas.start().await;
    engine_nas
        .add_folder(nas.folder(SyncOptions::default()))
        .unwrap();
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_phone = phone.start().await;
    engine_phone.add_folder(phone.folder(BACKUP_ONLY)).unwrap();
    engine_phone.scan(FOLDER).await.unwrap();
    engine_phone.connect(engine_nas.addr()).await.unwrap();

    eventually("the phone's photo reaches the NAS", || {
        nas.read("DCIM/one.jpg").as_deref() == Some(b"from the phone".as_slice())
    })
    .await;
    settle(&engine_phone, "desktop.txt").await;
    assert!(phone.read("desktop.txt").is_none(), "nothing is downloaded");
    assert_eq!(engine_phone.status(FOLDER).unwrap().pending, 0);

    // A different file the phone makes at the same path is kept beside the NAS's, never in its
    // place.
    phone.write("desktop.txt", b"the phone's own");
    engine_phone.scan(FOLDER).await.unwrap();
    eventually("both files are on the NAS", || {
        engine_nas.files(FOLDER).unwrap().len() == 3
    })
    .await;
    let mut contents: Vec<Vec<u8>> = Vec::new();
    eventually("the NAS wrote both", || {
        contents = engine_nas
            .files(FOLDER)
            .unwrap()
            .iter()
            .filter(|f| f.path.starts_with("desktop"))
            .filter_map(|f| nas.read(&f.path))
            .collect();
        contents.len() == 2
    })
    .await;
    contents.sort();
    assert_eq!(
        contents,
        vec![b"from the NAS".to_vec(), b"the phone's own".to_vec()]
    );
    fs::remove_file(phone.root.join("desktop.txt")).unwrap();
    engine_phone.scan(FOLDER).await.unwrap();

    // Changes and deletes on the NAS leave the phone's file alone.
    nas.write("DCIM/one.jpg", b"edited on the NAS");
    engine_nas.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        phone.read("DCIM/one.jpg").as_deref(),
        Some(b"from the phone".as_slice())
    );
    fs::remove_file(nas.root.join("DCIM/one.jpg")).unwrap();
    engine_nas.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(phone.read("DCIM/one.jpg").is_some());

    // The phone still sends new photos, and a send-only replica never confirms copies as a
    // full replica would.
    phone.write("DCIM/two.jpg", b"second");
    engine_phone.scan(FOLDER).await.unwrap();
    eventually("the second photo reaches the NAS", || {
        nas.read("DCIM/two.jpg").is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_secs(12)).await;
    assert!(engine_nas
        .files(FOLDER)
        .unwrap()
        .iter()
        .all(|f| f.confirmed_copies == 0));

    engine_phone.shutdown().await;
    engine_nas.shutdown().await;
}

/// Opening a file of a send-only folder downloads it once (ADR 0028); later versions from other
/// devices still leave it alone.
#[tokio::test(flavor = "multi_thread")]
async fn send_only_folders_download_what_is_opened() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([6; 32]);
    let phone = Device::new(tmp.path(), "phone", 46, &alice);
    let nas = Device::new(tmp.path(), "nas", 47, &alice);
    nas.write("photos/old.jpg", b"from the NAS");
    let engine_nas = nas.start().await;
    engine_nas
        .add_folder(nas.folder(SyncOptions::default()))
        .unwrap();
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_phone = phone.start().await;
    engine_phone.add_folder(phone.folder(BACKUP_ONLY)).unwrap();
    engine_phone.connect(engine_nas.addr()).await.unwrap();
    settle(&engine_phone, "photos/old.jpg").await;
    assert!(phone.read("photos/old.jpg").is_none());

    let opened = engine_phone.open(FOLDER, "photos/old.jpg").await.unwrap();
    engine_phone.close(FOLDER, "photos/old.jpg").unwrap();
    assert_eq!(opened, phone.root.join("photos/old.jpg"));
    assert_eq!(fs::read(&opened).unwrap(), b"from the NAS");
    assert!(on_disk(&engine_phone, "photos/old.jpg"));

    // A new version on the NAS does not replace the downloaded copy, and the copy is not sent
    // back as a change.
    nas.write("photos/old.jpg", b"edited on the NAS");
    engine_nas.scan(FOLDER).await.unwrap();
    engine_phone.scan(FOLDER).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        phone.read("photos/old.jpg").as_deref(),
        Some(b"from the NAS".as_slice())
    );
    assert_eq!(
        nas.read("photos/old.jpg").as_deref(),
        Some(b"edited on the NAS".as_slice())
    );
    assert_eq!(engine_nas.files(FOLDER).unwrap().len(), 1);

    engine_phone.shutdown().await;
    engine_nas.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn removed_files_wait_in_the_recycle_bin() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([5; 32]);
    let phone = Device::new(tmp.path(), "phone", 44, &alice);
    let nas = Device::new(tmp.path(), "nas", 45, &alice);
    phone.write("DCIM/a.jpg", b"first photo");
    phone.write("DCIM/b.jpg", b"second photo");
    let engine_nas = nas.start().await;
    engine_nas
        .add_folder(nas.folder(SyncOptions::default()))
        .unwrap();
    let engine_phone = phone.start().await;
    engine_phone.add_folder(phone.folder(TWO_WAY)).unwrap();
    engine_phone.scan(FOLDER).await.unwrap();
    engine_phone.connect(engine_nas.addr()).await.unwrap();
    eventually("the NAS confirms both photos", || {
        engine_phone
            .files(FOLDER)
            .unwrap()
            .iter()
            .all(|f| f.confirmed_copies == 1)
    })
    .await;

    // Removing a photo here moves it into the recycle bin; the NAS keeps it.
    engine_phone
        .remove_here(FOLDER, "DCIM/a.jpg")
        .await
        .unwrap();
    assert!(phone.read("DCIM/a.jpg").is_none());
    assert_eq!(phone.set_aside_files(), 1);
    assert!(phone.root.join(".tidefell/.nomedia").exists());
    let bin = engine_phone.recycle_bin(FOLDER).unwrap();
    assert_eq!(bin.len(), 1);
    assert!(bin[0].here_only);
    assert_eq!(bin[0].path, "DCIM/a.jpg");
    assert_eq!(bin[0].size, 11);
    assert!(bin[0].expires_at_ms.is_some());
    let summary = engine_phone.folders()[0].recycle_bin;
    assert_eq!((summary.items, summary.bytes), (1, 11));
    assert!(!on_disk(&engine_phone, "DCIM/a.jpg"));
    assert_eq!(engine_phone.status(FOLDER).unwrap().pending, 0);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(phone.read("DCIM/a.jpg").is_none(), "not downloaded again");
    assert_eq!(
        nas.read("DCIM/a.jpg").as_deref(),
        Some(b"first photo".as_slice())
    );
    assert!(engine_nas.recycle_bin(FOLDER).unwrap().is_empty());
    assert!(matches!(
        engine_phone.remove_here(FOLDER, "DCIM/a.jpg").await,
        Err(Error::NoSuchFile(_))
    ));

    // Restoring puts it back here only.
    engine_phone.restore(FOLDER, bin[0].entry).await.unwrap();
    assert_eq!(
        phone.read("DCIM/a.jpg").as_deref(),
        Some(b"first photo".as_slice())
    );
    assert!(on_disk(&engine_phone, "DCIM/a.jpg"));
    assert!(engine_phone.recycle_bin(FOLDER).unwrap().is_empty());
    assert_eq!(phone.set_aside_files(), 0);
    engine_phone.scan(FOLDER).await.unwrap();
    assert!(
        engine_nas.recycle_bin(FOLDER).unwrap().is_empty(),
        "restoring is not a change"
    );

    // Emptying the bin deletes the file for good, here only.
    engine_phone
        .remove_here(FOLDER, "DCIM/a.jpg")
        .await
        .unwrap();
    let entry = engine_phone.recycle_bin(FOLDER).unwrap()[0].entry;
    let removed = engine_phone.empty_recycle_bin(FOLDER, Some(entry)).unwrap();
    assert_eq!(removed.len(), 1);
    assert!(removed[0].here_only);
    assert_eq!(phone.set_aside_files(), 0);
    assert!(nas.read("DCIM/a.jpg").is_some());

    // Retention purges set-aside files too.
    engine_phone
        .remove_here(FOLDER, "DCIM/b.jpg")
        .await
        .unwrap();
    assert_eq!(phone.set_aside_files(), 1);
    let expires = engine_phone.recycle_bin(FOLDER).unwrap()[0]
        .expires_at_ms
        .unwrap();
    engine_phone.purge(expires - 1);
    assert_eq!(phone.set_aside_files(), 1, "not expired yet");
    engine_phone.purge(expires);
    assert_eq!(phone.set_aside_files(), 0);
    assert!(engine_phone.recycle_bin(FOLDER).unwrap().is_empty());
    assert!(nas.read("DCIM/b.jpg").is_some());

    engine_phone.shutdown().await;
    engine_nas.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_options_change_and_survive_restarts() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([6; 32]);
    let phone = Device::new(tmp.path(), "phone", 46, &alice);
    phone.write("kept.txt", b"kept");
    let engine = phone.start().await;
    engine
        .add_folder(phone.folder(SyncOptions::default()))
        .unwrap();
    engine.scan(FOLDER).await.unwrap();
    assert_eq!(engine.folders()[0].sync, SyncOptions::default());

    engine.set_sync_options(FOLDER, BACKUP_ONLY).unwrap();
    assert_eq!(engine.folders()[0].sync, BACKUP_ONLY);
    engine.shutdown().await;

    let engine = phone.start().await;
    assert_eq!(engine.folders()[0].sync, BACKUP_ONLY);
    // With local deletes on this device, deleting by hand records no delete.
    fs::remove_file(phone.root.join("kept.txt")).unwrap();
    engine.scan(FOLDER).await.unwrap();
    assert!(listed(&engine, "kept.txt"));
    assert!(engine.recycle_bin(FOLDER).unwrap().is_empty());

    // Putting the file back by hand makes it this device's again.
    phone.write("kept.txt", b"kept");
    engine.scan(FOLDER).await.unwrap();
    assert!(on_disk(&engine, "kept.txt"));
    engine.shutdown().await;
}
