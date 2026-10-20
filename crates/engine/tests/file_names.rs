//! File names across filesystems (ADR 0033): two engines with real iroh endpoints on localhost
//! and real folders on disk.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tidefell_engine::{
    Engine, EngineConfig, Filesystem, FolderConfig, NameLength, ProblemKind, RelayConfig,
    ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([33; 32]);

/// "résumé" with "é" as one code point (NFC) and as "e" with a combining accent (NFD).
const NFC: &str = "r\u{e9}sum\u{e9}.txt";
const NFD: &str = "re\u{301}sume\u{301}.txt";

struct Replica {
    root: PathBuf,
    data: PathBuf,
    key: DeviceKey,
}

impl Replica {
    fn new(dir: &Path, name: &str, seed: u8) -> Self {
        Self {
            root: dir.join(name).join("folder"),
            data: dir.join(name).join("data"),
            key: DeviceKey::from_seed([seed; 32]),
        }
    }

    async fn start(&self, user: &UserKey) -> Engine {
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
            folders: vec![FolderConfig {
                id: FOLDER,
                path: self.root.clone(),
                mode: ReplicaMode::Full,
                members: vec![user.id()],
                retention: Retention::Days(30),
                // Scans are triggered explicitly so the tests are deterministic.
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
                confirm_deletes: Default::default(),
                encryption: None,
            }],
            min_free_space: None,
        };
        let certificate = user.certify(self.key.id(), "test", 0);
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

    /// The names of the files at the top of the folder, as they are on disk.
    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(&self.root)
            .unwrap()
            .flatten()
            .filter(|e| e.file_type().unwrap().is_file())
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn paths(engine: &Engine) -> Vec<String> {
    engine
        .files(FOLDER)
        .unwrap()
        .into_iter()
        .map(|f| f.path)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn names_are_recorded_in_nfc_and_keep_their_spelling_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    // A filesystem that keeps names as given, as Linux does, holding a name in NFD.
    a.write(NFD, b"coffee");

    let engine_a = a.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(paths(&engine_a), [NFC]);
    let file = engine_a.files(FOLDER).unwrap().remove(0);
    assert!(file.on_disk, "the file on disk is the recorded one");
    // Scanned again, nothing changed.
    let status = engine_a.status(FOLDER).unwrap();
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(engine_a.status(FOLDER).unwrap().files, status.files);
    assert_eq!(a.names(), [NFD], "the name keeps its spelling on disk");

    // Another device writes the name as recorded.
    let engine_b = b.start(&user).await;
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("the file reaches b", || {
        b.read(NFC).as_deref() == Some(b"coffee".as_slice())
    })
    .await;
    assert_eq!(b.names(), [NFC]);

    // An edit on b is written to a under a's spelling, and is not a second file.
    b.write(NFC, b"tea");
    engine_b.scan(FOLDER).await.unwrap();
    eventually("the edit reaches a", || {
        a.read(NFD).as_deref() == Some(b"tea".as_slice())
    })
    .await;
    assert_eq!(a.names(), [NFD]);
    assert_eq!(paths(&engine_a), [NFC]);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_name_written_two_ways_is_one_file_and_a_problem() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    a.write(NFD, b"first");
    let engine_a = a.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();

    // The same name in NFC beside it, which Linux allows.
    a.write(NFC, b"second");
    engine_a.scan(FOLDER).await.unwrap();
    assert_eq!(paths(&engine_a), [NFC]);
    assert_eq!(
        engine_a.files(FOLDER).unwrap()[0].size,
        5,
        "the file it had keeps the path"
    );
    let problems = engine_a.path_problems(FOLDER).unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].path, NFC);
    assert_eq!(problems[0].kind, ProblemKind::NameNotAllowed);
}

// The temporary directory's filesystem must keep every name, as Linux's do; NTFS ignores case.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn the_doctor_reports_what_the_drive_does_with_names() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let engine = a.start(&user).await;
    // The test machine keeps every name as given.
    assert!(!engine.filesystem(FOLDER).unwrap().case_insensitive);
    let doctor = tokio::task::spawn_blocking(move || {
        let checks = engine.doctor();
        (engine, checks)
    })
    .await
    .unwrap()
    .1;
    let check = doctor.iter().find(|c| c.name == "filesystem").unwrap();
    assert!(
        check
            .detail
            .contains("names differing in case are different files")
            || check
                .detail
                .contains("Names differing in case are different files"),
        "{check:?}"
    );
}

/// A drive like FAT: names match without regard to case, and `:` and the like are refused.
fn fat() -> Filesystem {
    Filesystem {
        case_insensitive: true,
        refused_characters: vec!['<', '>', ':', '"', '|', '?', '*'],
        no_trailing_dot_or_space: true,
        max_name: Some(NameLength::Characters(255)),
        ..Filesystem::default()
    }
}

/// What each file at the top of the folder holds, by its name on disk.
#[cfg(target_os = "linux")]
fn contents(replica: &Replica) -> Vec<(String, String)> {
    replica
        .names()
        .into_iter()
        .map(|n| {
            let text = String::from_utf8(replica.read(&n).unwrap()).unwrap();
            (n, text)
        })
        .collect()
}

// Names that differ only in case come from a filesystem that keeps both, as Linux's do.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn a_drive_that_ignores_case_keeps_both_files_and_lists_refused_names() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    a.write("Photo.jpg", b"upper");
    a.write("photo.jpg", b"lower");
    a.write("a:b.txt", b"colon");
    a.write("ok.txt", b"fine");
    let engine_a = a.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    let engine_b = b.start(&user).await;
    engine_b.assume_filesystem(FOLDER, fat()).unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();

    eventually("both photos and ok.txt on b", || b.names().len() == 3).await;
    // Run a few more passes, as a device that keeps running would.
    for _ in 0..3 {
        engine_b.materialize(FOLDER).await.unwrap();
        engine_b.scan(FOLDER).await.unwrap();
    }
    let on_b = contents(&b);
    let texts: Vec<&str> = on_b.iter().map(|(_, t)| t.as_str()).collect();
    assert!(
        texts.contains(&"upper") && texts.contains(&"lower"),
        "{on_b:?}"
    );
    assert!(
        on_b.iter().any(|(n, _)| n.contains("(conflict ")),
        "{on_b:?}"
    );
    assert!(b.read("a:b.txt").is_none());

    // The refused name is listed, not on b, with one problem saying why.
    let colon = engine_b
        .files(FOLDER)
        .unwrap()
        .into_iter()
        .find(|f| f.path == "a:b.txt")
        .unwrap();
    assert!(!colon.on_disk);
    let problems = engine_b.path_problems(FOLDER).unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].path, "a:b.txt");
    assert_eq!(problems[0].kind, ProblemKind::NameNotAllowed);
    assert_eq!(engine_b.status(FOLDER).unwrap().pending, 0);

    // a keeps both paths as they were, and b recorded nothing of its own.
    assert_eq!(
        contents(&a),
        [
            ("Photo.jpg".to_owned(), "upper".to_owned()),
            ("a:b.txt".to_owned(), "colon".to_owned()),
            ("ok.txt".to_owned(), "fine".to_owned()),
            ("photo.jpg".to_owned(), "lower".to_owned()),
        ]
    );
    let mut on_a = paths(&engine_a);
    on_a.sort();
    assert_eq!(on_a, ["Photo.jpg", "a:b.txt", "ok.txt", "photo.jpg"]);
    let status = engine_a.status(FOLDER).unwrap();
    assert!(
        status.files.keys().all(|p| !p.contains("conflict")),
        "{:?}",
        status.files
    );

    // An edit on b to the file that gave way there keeps its name everywhere.
    let (moved, _) = on_b.iter().find(|(n, _)| n.contains("(conflict ")).unwrap();
    let original = if moved.starts_with("Photo") {
        "Photo.jpg"
    } else {
        "photo.jpg"
    };
    b.write(moved, b"edited on b");
    engine_b.scan(FOLDER).await.unwrap();
    eventually("the edit reaches a under its own name", || {
        a.read(original).as_deref() == Some(b"edited on b".as_slice())
    })
    .await;
    let mut on_a = paths(&engine_a);
    on_a.sort();
    assert_eq!(on_a, ["Photo.jpg", "a:b.txt", "ok.txt", "photo.jpg"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn renames_that_only_change_case() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    a.write("Notes.txt", b"notes");
    let engine_a = a.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    let engine_b = b.start(&user).await;
    engine_b.assume_filesystem(FOLDER, fat()).unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("Notes.txt on b", || b.read("Notes.txt").is_some()).await;

    // Renamed elsewhere in case only: renamed here too, not written again or deleted.
    engine_a
        .rename(FOLDER, "Notes.txt", "notes.txt")
        .await
        .unwrap();
    eventually("the rename reaches b", || b.names() == ["notes.txt"]).await;
    assert_eq!(b.read("notes.txt").as_deref(), Some(b"notes".as_slice()));
    assert_eq!(paths(&engine_b), ["notes.txt"]);

    // Renamed here in case only: a rename everywhere, not a second file.
    fs::rename(b.root.join("notes.txt"), b.root.join("NOTES.txt")).unwrap();
    engine_b.scan(FOLDER).await.unwrap();
    assert_eq!(paths(&engine_b), ["NOTES.txt"]);
    eventually("the rename reaches a", || a.names() == ["NOTES.txt"]).await;
    assert_eq!(paths(&engine_a), ["NOTES.txt"]);
    assert_eq!(a.read("NOTES.txt").as_deref(), Some(b"notes".as_slice()));
}
