//! Partial replicas (ADR 0016): placeholders, fetching on open, writes, and eviction to the
//! budget.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, Error, FolderConfig, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([9; 32]);

struct Device {
    dir: PathBuf,
    key: DeviceKey,
    user: UserKey,
    mode: ReplicaMode,
}

impl Device {
    fn new(root: &Path, name: &str, seed: u8, user: &UserKey, mode: ReplicaMode) -> Self {
        Self {
            dir: root.join(name),
            key: DeviceKey::from_seed([seed; 32]),
            user: user.clone(),
            mode,
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.join("folder")
    }

    async fn start(&self) -> Engine {
        self.start_with(&[]).await
    }

    /// Starts with `others` as members too.
    async fn start_with(&self, others: &[&UserKey]) -> Engine {
        let config = EngineConfig {
            data_dir: self.dir.join("data"),
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
                path: self.root(),
                mode: self.mode,
                members: std::iter::once(self.user.id())
                    .chain(others.iter().map(|u| u.id()))
                    .collect(),
                retention: Retention::Days(30),
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
                confirm_deletes: Default::default(),
                encryption: None,
            }],
            min_free_space: None,
        };
        let certificate = self.user.certify(self.key.id(), "test", 0);
        Engine::start(config, self.key.clone(), certificate)
            .await
            .unwrap()
    }

    fn read(&self, path: &str) -> Option<Vec<u8>> {
        fs::read(self.root().join(path)).ok()
    }

    fn write(&self, path: &str, data: &[u8]) {
        fs::create_dir_all(self.root()).unwrap();
        fs::write(self.root().join(path), data).unwrap();
    }
}

async fn eventually(what: &str, check: impl FnMut() -> bool) {
    eventually_within(what, Duration::from_secs(20), check).await;
}

async fn eventually_within(what: &str, limit: Duration, mut check: impl FnMut() -> bool) {
    for _ in 0..limit.as_millis() / 20 {
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

/// Opens and closes `path`, then waits a little so last-use times differ.
async fn touch(engine: &Engine, path: &str) -> Vec<u8> {
    let local = engine.open(FOLDER, path).await.unwrap();
    let data = fs::read(local).unwrap();
    engine.close(FOLDER, path).unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    data
}

#[tokio::test(flavor = "multi_thread")]
async fn placeholders_open_on_demand_and_evict_to_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let nas = Device::new(tmp.path(), "nas", 20, &alice, ReplicaMode::Full);
    let phone = Device::new(
        tmp.path(),
        "phone",
        21,
        &alice,
        ReplicaMode::Partial { budget_bytes: 25 },
    );
    nas.write("a.txt", b"aaaaaaaaaa");
    nas.write("b.txt", b"bbbbbbbbbb");
    nas.write("c.txt", b"cccccccccc");

    let engine_nas = nas.start().await;
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_phone = phone.start().await;
    engine_phone.connect(engine_nas.addr()).await.unwrap();

    // Metadata arrives; content stays on the NAS.
    eventually("phone lists the files", || {
        engine_phone.files(FOLDER).unwrap().len() == 3
    })
    .await;
    engine_phone.materialize(FOLDER).await.unwrap();
    assert!(phone.read("a.txt").is_none());
    assert!(!on_disk(&engine_phone, "a.txt"));
    assert_eq!(engine_phone.status(FOLDER).unwrap().pending, 0);
    assert!(matches!(
        engine_phone.open(FOLDER, "missing.txt").await,
        Err(Error::NoSuchFile(_))
    ));

    // Opening fetches.
    assert_eq!(touch(&engine_phone, "a.txt").await, b"aaaaaaaaaa");
    assert!(on_disk(&engine_phone, "a.txt"));
    assert_eq!(touch(&engine_phone, "b.txt").await, b"bbbbbbbbbb");

    // A third file exceeds the 25-byte budget: the least recently used, a.txt, is evicted.
    let c = engine_phone.open(FOLDER, "c.txt").await.unwrap();
    assert_eq!(fs::read(&c).unwrap(), b"cccccccccc");
    assert!(phone.read("a.txt").is_none());
    assert!(!on_disk(&engine_phone, "a.txt"));
    assert!(on_disk(&engine_phone, "b.txt"));

    // Open files are never evicted: with c.txt pinned, opening a.txt again evicts b.txt.
    assert_eq!(touch(&engine_phone, "a.txt").await, b"aaaaaaaaaa");
    assert!(on_disk(&engine_phone, "c.txt"));
    assert!(!on_disk(&engine_phone, "b.txt"));
    engine_phone.close(FOLDER, "c.txt").unwrap();

    // A remote edit updates a materialised file and leaves placeholders alone.
    nas.write("a.txt", b"AAAAAAAAAA");
    nas.write("b.txt", b"BBBBBBBBBB");
    engine_nas.scan(FOLDER).await.unwrap();
    eventually("phone updates its copy of a.txt", || {
        phone.read("a.txt").as_deref() == Some(b"AAAAAAAAAA".as_slice())
    })
    .await;
    engine_phone.materialize(FOLDER).await.unwrap();
    assert!(phone.read("b.txt").is_none());

    // A new file written through the engine reaches the NAS.
    let source = tmp.path().join("upload.tmp");
    fs::write(&source, b"from the phone").unwrap();
    engine_phone
        .write(FOLDER, "photos/new.txt", &source)
        .await
        .unwrap();
    assert!(!source.exists(), "the source is moved into the folder");
    eventually("the NAS has the phone's file", || {
        nas.read("photos/new.txt").as_deref() == Some(b"from the phone".as_slice())
    })
    .await;

    engine_phone.shutdown().await;
    engine_nas.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn files_made_here_are_kept_until_a_full_replica_has_them() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([2; 32]);
    let phone = Device::new(
        tmp.path(),
        "phone",
        30,
        &alice,
        // Fits either file alone but not both, so a background eviction pass keeps the
        // most recently used one.
        ReplicaMode::Partial { budget_bytes: 20 },
    );
    phone.write("one.txt", b"first local file");
    phone.write("two.txt", b"second local file");
    let engine_phone = phone.start().await;
    engine_phone.scan(FOLDER).await.unwrap();

    // Over budget, but no peer holds either file: nothing is evicted.
    touch(&engine_phone, "two.txt").await;
    assert!(phone.read("one.txt").is_some());
    assert!(phone.read("two.txt").is_some());

    // A partial replica holding them does not count (ADR 0022): it may evict them in turn.
    let tablet = Device::new(
        tmp.path(),
        "tablet",
        32,
        &alice,
        ReplicaMode::Partial {
            budget_bytes: 1 << 20,
        },
    );
    let engine_tablet = tablet.start().await;
    engine_tablet.connect(engine_phone.addr()).await.unwrap();
    eventually("the tablet lists both files", || {
        engine_tablet.files(FOLDER).unwrap().len() == 2
    })
    .await;
    touch(&engine_tablet, "one.txt").await;
    touch(&engine_tablet, "two.txt").await;
    // Longer than the engine's confirmation interval.
    tokio::time::sleep(Duration::from_secs(12)).await;
    touch(&engine_phone, "two.txt").await;
    assert!(
        phone.read("one.txt").is_some(),
        "kept while only a partial replica has it"
    );
    assert_eq!(engine_phone.folders()[0].only_here, 2);
    assert!(engine_phone
        .files(FOLDER)
        .unwrap()
        .iter()
        .all(|f| f.only_here));
    assert!(engine_phone
        .files(FOLDER)
        .unwrap()
        .iter()
        .all(|f| f.confirmed_copies == 0));
    engine_tablet.shutdown().await;

    // Once a full replica confirms them, the least recently used one goes.
    let nas = Device::new(tmp.path(), "nas", 31, &alice, ReplicaMode::Full);
    let engine_nas = nas.start().await;
    engine_nas.connect(engine_phone.addr()).await.unwrap();
    eventually("the NAS has both files", || {
        nas.read("one.txt").is_some() && nas.read("two.txt").is_some()
    })
    .await;
    eventually_within(
        "the NAS's copies are confirmed",
        Duration::from_secs(30),
        || {
            engine_phone
                .files(FOLDER)
                .unwrap()
                .iter()
                .all(|f| f.confirmed_copies == 1)
        },
    )
    .await;
    assert_eq!(engine_phone.folders()[0].only_here, 0);
    let copies = engine_phone.copies(FOLDER, "one.txt").unwrap();
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].device, engine_nas.id());
    assert_eq!(copies[0].name, "test");
    assert!(matches!(
        engine_phone.copies(FOLDER, "missing.txt"),
        Err(Error::NoSuchFile(_))
    ));
    touch(&engine_phone, "two.txt").await;
    assert!(phone.read("one.txt").is_none(), "one.txt evicted");
    assert!(phone.read("two.txt").is_some());
    assert!(!on_disk(&engine_phone, "one.txt"));

    // An evicted file can be opened again from the NAS.
    assert_eq!(touch(&engine_phone, "one.txt").await, b"first local file");

    engine_nas.shutdown().await;
    engine_phone.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_and_rename_work_on_placeholders() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let nas = Device::new(tmp.path(), "nas", 30, &alice, ReplicaMode::Full);
    let laptop = Device::new(
        tmp.path(),
        "laptop",
        31,
        &alice,
        ReplicaMode::Partial {
            budget_bytes: 1 << 20,
        },
    );
    nas.write("a.txt", b"first");
    nas.write("old.txt", b"replaced");
    fs::create_dir_all(nas.root().join("docs")).unwrap();
    nas.write("docs/b.txt", b"bee");
    nas.write("docs/c.txt", b"sea");

    let engine_nas = nas.start().await;
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_laptop = laptop.start().await;
    engine_laptop.connect(engine_nas.addr()).await.unwrap();
    eventually("laptop lists the files", || {
        engine_laptop.files(FOLDER).unwrap().len() == 4
    })
    .await;

    // Renaming a placeholder moves it on the NAS without fetching it here.
    engine_laptop
        .rename(FOLDER, "a.txt", "z.txt")
        .await
        .unwrap();
    assert!(!on_disk(&engine_laptop, "z.txt"));
    eventually("the NAS moves a.txt", || {
        nas.read("z.txt").as_deref() == Some(b"first".as_slice()) && nas.read("a.txt").is_none()
    })
    .await;

    // Renaming a directory moves materialised files on disk and placeholders in metadata.
    assert_eq!(touch(&engine_laptop, "docs/b.txt").await, b"bee");
    engine_laptop
        .rename(FOLDER, "docs", "archive")
        .await
        .unwrap();
    assert_eq!(
        laptop.read("archive/b.txt").as_deref(),
        Some(b"bee".as_slice())
    );
    assert!(laptop.read("docs/b.txt").is_none());
    assert!(on_disk(&engine_laptop, "archive/b.txt"));
    assert!(!on_disk(&engine_laptop, "archive/c.txt"));
    eventually("the NAS moves the directory", || {
        nas.read("archive/b.txt").is_some()
            && nas.read("archive/c.txt").as_deref() == Some(b"sea".as_slice())
            && nas.read("docs/c.txt").is_none()
    })
    .await;

    // Renaming over a file replaces it, and the replaced version goes to the recycle bin.
    engine_laptop
        .rename(FOLDER, "z.txt", "old.txt")
        .await
        .unwrap();
    eventually("the NAS replaces old.txt", || {
        nas.read("old.txt").as_deref() == Some(b"first".as_slice()) && nas.read("z.txt").is_none()
    })
    .await;
    eventually("the replaced file is in the NAS recycle bin", || {
        engine_nas
            .recycle_bin(FOLDER)
            .unwrap()
            .iter()
            .any(|r| r.path == "old.txt")
    })
    .await;

    // Deleting a placeholder deletes it everywhere.
    engine_laptop.delete(FOLDER, "archive/c.txt").await.unwrap();
    assert!(engine_laptop
        .files(FOLDER)
        .unwrap()
        .iter()
        .all(|f| f.path != "archive/c.txt"));
    eventually("the NAS deletes archive/c.txt", || {
        nas.read("archive/c.txt").is_none()
    })
    .await;

    // Deleting a materialised file removes it from disk here too.
    engine_laptop.delete(FOLDER, "archive/b.txt").await.unwrap();
    assert!(laptop.read("archive/b.txt").is_none());
    eventually("the NAS deletes archive/b.txt", || {
        nas.read("archive/b.txt").is_none()
    })
    .await;

    assert!(matches!(
        engine_laptop.delete(FOLDER, "nope.txt").await,
        Err(Error::NoSuchFile(_))
    ));
    assert!(matches!(
        engine_laptop.rename(FOLDER, "nope", "elsewhere").await,
        Err(Error::NoSuchFile(_))
    ));
    assert!(matches!(
        engine_laptop
            .rename(FOLDER, "old.txt", "old.txt/inside")
            .await,
        Err(Error::Invalid(_))
    ));

    engine_laptop.shutdown().await;
    engine_nas.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pinned_directory_stays_over_the_budget() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([3; 32]);
    let full = Device::new(dir.path(), "full", 40, &user, ReplicaMode::Full);
    let partial = Device::new(
        dir.path(),
        "partial",
        41,
        &user,
        ReplicaMode::Partial { budget_bytes: 10 },
    );
    fs::create_dir_all(full.root().join("keep")).unwrap();
    full.write("keep/a.txt", &[b'a'; 100]);
    full.write("keep/b.txt", &[b'b'; 100]);
    full.write("other.txt", &[b'o'; 100]);
    let engine_full = full.start().await;
    engine_full.scan(FOLDER).await.unwrap();
    let engine = partial.start().await;
    engine.connect(engine_full.addr()).await.unwrap();
    eventually("the partial replica lists every file", || {
        engine.files(FOLDER).unwrap().len() == 3
    })
    .await;
    assert!(
        partial.read("keep/a.txt").is_none(),
        "placeholders until pinned"
    );

    // Pinned files arrive without being opened.
    engine.pin(FOLDER, "keep/").await.unwrap();
    eventually("the pinned directory is fetched", || {
        partial.read("keep/a.txt").is_some() && partial.read("keep/b.txt").is_some()
    })
    .await;
    assert_eq!(engine.pins(FOLDER).unwrap(), ["keep"]);
    assert_eq!(engine.folders()[0].pinned_bytes, 200);
    let pinned: Vec<bool> = engine
        .files(FOLDER)
        .unwrap()
        .iter()
        .map(|f| f.pinned)
        .collect();
    assert_eq!(pinned, [true, true, false]);

    // An ordinary file opened and closed is evicted to the 10-byte budget; the pinned ones,
    // 200 bytes, stay.
    engine.open(FOLDER, "other.txt").await.unwrap();
    engine.close(FOLDER, "other.txt").unwrap();
    engine.pin(FOLDER, "keep").await.unwrap();
    eventually("the ordinary file is evicted", || {
        partial.read("other.txt").is_none()
    })
    .await;
    assert!(partial.read("keep/a.txt").is_some());
    assert!(partial.read("keep/b.txt").is_some());

    // Pins are kept across a restart, and unpinning lets them be evicted.
    engine.shutdown().await;
    let engine = partial.start().await;
    assert_eq!(engine.pins(FOLDER).unwrap(), ["keep"]);
    engine.unpin(FOLDER, "keep").await.unwrap();
    assert!(partial.read("keep/a.txt").is_none() || partial.read("keep/b.txt").is_none());
    assert!(engine.unpin(FOLDER, "keep").await.is_err());

    // A full replica keeps every file already.
    assert!(matches!(
        engine_full.pin(FOLDER, "keep").await,
        Err(Error::Invalid(_))
    ));

    // Pins are this device's choice, kept apart from the folder's metadata: a rebuild keeps
    // them, and so does losing the database (ADR 0034).
    engine.pin(FOLDER, "keep").await.unwrap();
    engine.rebuild_folder(FOLDER).await.unwrap();
    assert_eq!(engine.pins(FOLDER).unwrap(), ["keep"]);
    engine.shutdown().await;
    for entry in fs::read_dir(partial.dir.join("data/metadata"))
        .unwrap()
        .flatten()
    {
        fs::remove_file(entry.path()).unwrap();
    }
    let engine = partial.start().await;
    assert_eq!(engine.pins(FOLDER).unwrap(), ["keep"]);
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_removed_members_copies_no_longer_count() {
    let tmp = tempfile::tempdir().unwrap();
    let (alice, bob) = (UserKey::from_seed([3; 32]), UserKey::from_seed([4; 32]));
    let phone = Device::new(tmp.path(), "phone", 40, &alice, ReplicaMode::Full);
    phone.write("one.txt", b"made on the phone");
    let engine_phone = phone.start_with(&[&bob]).await;
    engine_phone.scan(FOLDER).await.unwrap();

    // Bob's NAS keeps the folder too, and confirms holding the phone's file.
    let nas = Device::new(tmp.path(), "nas", 41, &bob, ReplicaMode::Full);
    let engine_nas = nas.start_with(&[&alice]).await;
    engine_nas.connect(engine_phone.addr()).await.unwrap();
    eventually_within(
        "the NAS's copy is confirmed",
        Duration::from_secs(30),
        || engine_phone.files(FOLDER).unwrap()[0].confirmed_copies == 1,
    )
    .await;
    assert_eq!(engine_phone.folders()[0].only_here, 0);
    engine_nas.shutdown().await;
    engine_phone.shutdown().await;

    // Once bob is no longer a member, his NAS's copy is not counted: the file is only here
    // again.
    let engine_phone = phone.start().await;
    let file = &engine_phone.files(FOLDER).unwrap()[0];
    assert_eq!(file.confirmed_copies, 0);
    assert!(file.only_here);
    assert!(engine_phone.copies(FOLDER, "one.txt").unwrap().is_empty());
    assert_eq!(engine_phone.folders()[0].only_here, 1);

    engine_phone.shutdown().await;

    // Bob back: it counts again, with nothing asked of the NAS, which is gone, as the phone
    // kept its certificate.
    let engine_phone = phone.start_with(&[&bob]).await;
    assert_eq!(engine_phone.files(FOLDER).unwrap()[0].confirmed_copies, 1);
    engine_phone.shutdown().await;
}

/// Fetching without placing hands over the content in order and leaves the folder as it was
/// (ADR 0042).
#[tokio::test(flavor = "multi_thread")]
async fn content_is_fetched_without_being_placed() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let nas = Device::new(tmp.path(), "nas", 50, &alice, ReplicaMode::Full);
    let laptop = Device::new(
        tmp.path(),
        "laptop",
        51,
        &alice,
        ReplicaMode::Partial {
            budget_bytes: 1 << 20,
        },
    );
    // Several chunks, so their order matters.
    let big: Vec<u8> = (0..3_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    nas.write("big.bin", &big);
    nas.write("small.txt", b"small");
    let engine_nas = nas.start().await;
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_laptop = laptop.start().await;
    engine_laptop.connect(engine_nas.addr()).await.unwrap();
    eventually("laptop lists the files", || {
        engine_laptop.files(FOLDER).unwrap().len() == 2
    })
    .await;

    for (path, content) in [("big.bin", &big[..]), ("small.txt", b"small".as_slice())] {
        let mut got = Vec::new();
        engine_laptop
            .fetch(FOLDER, path, None, &mut |offset, data| {
                assert_eq!(offset, got.len() as u64);
                got.extend_from_slice(data);
                Ok(())
            })
            .await
            .unwrap();
        assert!(got == content, "{path}");
        assert!(!on_disk(&engine_laptop, path));
        assert!(laptop.read(path).is_none());
    }
    assert!(matches!(
        engine_laptop
            .fetch(FOLDER, "missing.txt", None, &mut |_, _| Ok(()))
            .await,
        Err(Error::NoSuchFile(_))
    ));

    // Put in place by another component, as Windows fills a placeholder, and looked at: the
    // same entry, now on disk, with nothing to sync.
    let entry = |e: &Engine| {
        e.files(FOLDER)
            .unwrap()
            .into_iter()
            .find(|f| f.path == "small.txt")
            .unwrap()
            .entry
    };
    let before = entry(&engine_laptop);
    laptop.write("small.txt", b"small");
    engine_laptop.scan_path(FOLDER, "small.txt").await.unwrap();
    assert!(on_disk(&engine_laptop, "small.txt"));
    assert_eq!(entry(&engine_laptop), before);
    assert_eq!(engine_nas.files(FOLDER).unwrap().len(), 2);
    engine_nas.shutdown().await;
    engine_laptop.shutdown().await;
}

/// A move the user already made on disk, reported afterwards, as a Windows sync root reports
/// moves (ADR 0042): the record follows the file, which is not looked for at its old path.
#[tokio::test(flavor = "multi_thread")]
async fn a_rename_already_made_on_disk_is_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let nas = Device::new(tmp.path(), "nas", 40, &alice, ReplicaMode::Full);
    let laptop = Device::new(
        tmp.path(),
        "laptop",
        41,
        &alice,
        ReplicaMode::Partial {
            budget_bytes: 1 << 20,
        },
    );
    fs::create_dir_all(nas.root().join("docs")).unwrap();
    nas.write("docs/b.txt", b"bee");
    nas.write("docs/c.txt", b"sea");
    let engine_nas = nas.start().await;
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_laptop = laptop.start().await;
    engine_laptop.connect(engine_nas.addr()).await.unwrap();
    eventually("laptop lists the files", || {
        engine_laptop.files(FOLDER).unwrap().len() == 2
    })
    .await;
    assert_eq!(touch(&engine_laptop, "docs/b.txt").await, b"bee");

    // The directory moved by the user, with the file on disk in it and the placeholder.
    fs::rename(laptop.root().join("docs"), laptop.root().join("papers")).unwrap();
    engine_laptop
        .rename(FOLDER, "docs", "papers")
        .await
        .unwrap();
    assert!(on_disk(&engine_laptop, "papers/b.txt"));
    assert!(!on_disk(&engine_laptop, "papers/c.txt"));
    eventually("the NAS moves the directory", || {
        nas.read("papers/b.txt").as_deref() == Some(b"bee".as_slice())
            && nas.read("papers/c.txt").as_deref() == Some(b"sea".as_slice())
            && nas.read("docs/b.txt").is_none()
    })
    .await;
    // A scan finds nothing deleted or new.
    engine_laptop.scan(FOLDER).await.unwrap();
    assert_eq!(engine_laptop.files(FOLDER).unwrap().len(), 2);
    assert!(on_disk(&engine_laptop, "papers/b.txt"));
    engine_nas.shutdown().await;
    engine_laptop.shutdown().await;
}

/// A recorded placeholder that is gone at a scan is a delete, as one the user deleted while the
/// daemon was off is, and it builds on the version the placeholder showed: an edit made
/// elsewhere since survives (ADR 0042).
#[tokio::test(flavor = "multi_thread")]
async fn a_recorded_placeholder_that_goes_is_a_delete_of_what_it_showed() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let nas = Device::new(tmp.path(), "nas", 60, &alice, ReplicaMode::Full);
    let laptop = Device::new(
        tmp.path(),
        "laptop",
        61,
        &alice,
        ReplicaMode::Partial {
            budget_bytes: 1 << 20,
        },
    );
    nas.write("gone.txt", b"gone");
    nas.write("edited.txt", b"before");
    let engine_nas = nas.start().await;
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_laptop = laptop.start().await;
    engine_laptop.connect(engine_nas.addr()).await.unwrap();
    eventually("laptop lists the files", || {
        engine_laptop.files(FOLDER).unwrap().len() == 2
    })
    .await;
    let content = |e: &Engine, path: &str| {
        e.files(FOLDER)
            .unwrap()
            .into_iter()
            .find(|f| f.path == path)
            .unwrap()
            .content
    };
    // Placeholders made for both (by the provider, on Windows) and recorded.
    for path in ["gone.txt", "edited.txt"] {
        engine_laptop
            .placeholder_made(FOLDER, path, content(&engine_laptop, path))
            .unwrap();
    }
    assert_eq!(engine_laptop.placeholders(FOLDER).unwrap().len(), 2);

    // Edited on the NAS after the laptop's placeholder showed the earlier version, and only
    // then both placeholders deleted on the laptop.
    nas.write("edited.txt", b"after, on the NAS");
    engine_nas.scan(FOLDER).await.unwrap();
    eventually("the edit reaches the laptop's records", || {
        engine_laptop
            .files(FOLDER)
            .unwrap()
            .iter()
            .any(|f| f.path == "edited.txt" && f.size == 17)
    })
    .await;
    engine_laptop.scan(FOLDER).await.unwrap();
    assert!(engine_laptop.placeholders(FOLDER).unwrap().is_empty());

    eventually("the NAS deletes the placeholder the laptop deleted", || {
        nas.read("gone.txt").is_none()
    })
    .await;
    // The edit survives a delete of the version before it.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        nas.read("edited.txt").as_deref(),
        Some(b"after, on the NAS".as_slice())
    );
    assert!(engine_laptop
        .files(FOLDER)
        .unwrap()
        .iter()
        .any(|f| f.path == "edited.txt"));
    engine_nas.shutdown().await;
    engine_laptop.shutdown().await;
}

/// Most of a folder's placeholders gone at once, as when a drive's folder is wiped while the
/// daemon is off, is held for confirmation like most of its files would be (ADR 0037, ADR 0042).
#[tokio::test(flavor = "multi_thread")]
async fn most_placeholders_gone_at_once_are_held() {
    const FILES: usize = 120;
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let nas = Device::new(tmp.path(), "nas", 62, &alice, ReplicaMode::Full);
    let laptop = Device::new(
        tmp.path(),
        "laptop",
        63,
        &alice,
        ReplicaMode::Partial {
            budget_bytes: 1 << 20,
        },
    );
    for i in 0..FILES {
        nas.write(&format!("f{i:03}.txt"), format!("file {i}").as_bytes());
    }
    let engine_nas = nas.start().await;
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_laptop = laptop.start().await;
    engine_laptop.connect(engine_nas.addr()).await.unwrap();
    eventually("laptop lists the files", || {
        engine_laptop.files(FOLDER).unwrap().len() == FILES
    })
    .await;
    for f in engine_laptop.files(FOLDER).unwrap() {
        engine_laptop
            .placeholder_made(FOLDER, &f.path, f.content)
            .unwrap();
    }
    engine_laptop.scan(FOLDER).await.unwrap();
    assert_eq!(engine_laptop.held_deletes(FOLDER).unwrap().len(), FILES);
    assert_eq!(engine_laptop.placeholders(FOLDER).unwrap().len(), FILES);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        engine_nas.files(FOLDER).unwrap().len(),
        FILES,
        "nothing deleted"
    );

    // Brought back: the placeholders are forgotten, for their provider to make again.
    engine_laptop.restore_deleted(FOLDER).await.unwrap();
    assert!(engine_laptop.held_deletes(FOLDER).unwrap().is_empty());
    assert!(engine_laptop.placeholders(FOLDER).unwrap().is_empty());
    engine_laptop.scan(FOLDER).await.unwrap();
    assert_eq!(engine_nas.files(FOLDER).unwrap().len(), FILES);
    engine_nas.shutdown().await;
    engine_laptop.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn eviction_frees_in_place_where_a_freer_is_set() {
    let tmp = tempfile::tempdir().unwrap();
    let alice = UserKey::from_seed([1; 32]);
    let nas = Device::new(tmp.path(), "nas", 40, &alice, ReplicaMode::Full);
    let laptop = Device::new(
        tmp.path(),
        "laptop",
        41,
        &alice,
        ReplicaMode::Partial { budget_bytes: 25 },
    );
    nas.write("a.txt", b"aaaaaaaaaa");
    nas.write("b.txt", b"bbbbbbbbbb");
    nas.write("c.txt", b"cccccccccc");
    let engine_nas = nas.start().await;
    engine_nas.scan(FOLDER).await.unwrap();
    let engine_laptop = laptop.start().await;
    engine_laptop.connect(engine_nas.addr()).await.unwrap();
    eventually("the laptop lists the files", || {
        engine_laptop.files(FOLDER).unwrap().len() == 3
    })
    .await;

    // Frees a.txt, as a sync root would dehydrate it, and refuses anything else.
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    engine_laptop.set_freer(
        FOLDER,
        Some(std::sync::Arc::new({
            let asked = asked.clone();
            move |path: &Path, entry, content| {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                asked.lock().unwrap().push((name.clone(), entry, content));
                Ok(name == "a.txt")
            }
        })),
    );
    assert_eq!(touch(&engine_laptop, "a.txt").await, b"aaaaaaaaaa");
    assert_eq!(touch(&engine_laptop, "b.txt").await, b"bbbbbbbbbb");
    let a = engine_laptop
        .files(FOLDER)
        .unwrap()
        .into_iter()
        .find(|f| f.path == "a.txt")
        .unwrap();
    assert_eq!(touch(&engine_laptop, "c.txt").await, b"cccccccccc");

    // a.txt, the least recently used, was freed: left where it is, recorded as a placeholder.
    assert_eq!(
        asked.lock().unwrap().as_slice(),
        &[("a.txt".to_owned(), a.entry, a.content)]
    );
    assert!(!on_disk(&engine_laptop, "a.txt"));
    assert!(
        laptop.read("a.txt").is_some(),
        "freed in place, not removed"
    );
    assert!(engine_laptop
        .placeholders(FOLDER)
        .unwrap()
        .iter()
        .any(|(path, _)| path == "a.txt"));

    // One the freer does not free is removed, as without one.
    engine_laptop.set_freer(FOLDER, None);
    fs::remove_file(laptop.root().join("a.txt")).unwrap();
    engine_laptop.placeholder_gone(FOLDER, "a.txt").unwrap();
    assert_eq!(touch(&engine_laptop, "a.txt").await, b"aaaaaaaaaa");
    assert!(laptop.read("b.txt").is_none());
    assert!(!on_disk(&engine_laptop, "b.txt"));

    engine_laptop.shutdown().await;
    engine_nas.shutdown().await;
}
