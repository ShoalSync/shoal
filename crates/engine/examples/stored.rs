//! Measures a storage peer (ADR 0023, ADR 0035): its resident memory and idle CPU once it stores
//! an encrypted folder of many versions.
//!
//! `cargo run --release -p shoal-engine --example stored -- run <files>` writes `files` small
//! files into a member's encrypted folder, then runs the member in this process and a storage
//! peer in another, over loopback. The storage peer reports its resident memory once it stores
//! every version and chunk, how long its status takes, and its CPU while idle. Then a second
//! member device, in a third process, which knows only the storage peer, joins the folder and
//! reports how long it took to have every file on disk. Linux only.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use shoal_engine::{
    Encryption, Engine, EngineConfig, FolderConfig, PeerAddr, RelayConfig, ReplicaMode, Retention,
    Stored,
};
use shoal_identity::{DeviceKey, UserKey};
use shoal_proto::FolderId;

#[global_allocator]
static ALLOCATOR: shoal_engine::Allocator = shoal_engine::Allocator;

const FOLDER: FolderId = FolderId([43; 32]);
const MEMBER_PORT: u16 = 47711;
const STORAGE_PORT: u16 = 47712;
const SECOND_PORT: u16 = 47713;

fn user() -> UserKey {
    UserKey::from_seed([1; 32])
}
fn member_key() -> DeviceKey {
    DeviceKey::from_seed([2; 32])
}
fn storage_key() -> DeviceKey {
    DeviceKey::from_seed([3; 32])
}
fn second_key() -> DeviceKey {
    DeviceKey::from_seed([4; 32])
}

fn peer(key: &DeviceKey, port: u16) -> PeerAddr {
    PeerAddr {
        device: key.id(),
        addrs: vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))],
        relay: None,
    }
}

fn config(data: PathBuf, port: u16, peers: Vec<PeerAddr>, folder: FolderConfig) -> EngineConfig {
    EngineConfig {
        data_dir: data,
        metadata_dir: None,
        relay: RelayConfig::Disabled,
        address_lookup: false,
        local_discovery: false,
        watch: false,
        limits: Default::default(),
        bind: Some(std::net::SocketAddr::from(([127, 0, 0, 1], port))),
        peers,
        folders: vec![folder],
        min_free_space: None,
    }
}

fn folder(path: PathBuf, encryption: Encryption) -> FolderConfig {
    FolderConfig {
        id: FOLDER,
        path,
        mode: ReplicaMode::Full,
        members: vec![user().id()],
        retention: Retention::Days(30),
        scan_interval: Duration::from_secs(3600),
        sync: Default::default(),
        confirm_deletes: Default::default(),
        encryption: Some(encryption),
    }
}

/// Resident memory of this process, in MiB.
fn resident() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kib| kib / 1024)
}

/// User and system CPU time of this process, in seconds.
fn cpu() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let fields: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect();
    let ticks: u64 = fields.get(11).and_then(|u| u.parse().ok()).unwrap_or(0)
        + fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);
    ticks as f64 / 100.0
}

async fn member(dir: &Path) {
    let key = member_key();
    let config = config(
        dir.join("member"),
        MEMBER_PORT,
        vec![peer(&storage_key(), STORAGE_PORT)],
        folder(
            dir.join("folder"),
            Encryption {
                storage_peers: vec![storage_key().id()],
                stored: None,
            },
        ),
    );
    let engine = Engine::start(config, key.clone(), user().certify(key.id(), "member", 0))
        .await
        .expect("the member starts");
    let started = Instant::now();
    engine.scan(FOLDER).await.expect("the folder scans");
    println!(
        "member   scanned and sealed in {:.0} s",
        started.elapsed().as_secs_f64()
    );
    // Serves the storage peer until it has measured and exited.
    std::future::pending::<()>().await;
}

async fn store(dir: &Path, files: usize) {
    let key = storage_key();
    let operator = UserKey::from_seed([9; 32]);
    let config = config(
        dir.join("storage"),
        STORAGE_PORT,
        vec![
            peer(&member_key(), MEMBER_PORT),
            peer(&second_key(), SECOND_PORT),
        ],
        folder(
            dir.join("storage").join("stored"),
            Encryption {
                storage_peers: Vec::new(),
                stored: Some(Stored {
                    quota_bytes: u64::MAX,
                    priority: Stored::DEFAULT_PRIORITY,
                }),
            },
        ),
    );
    let engine = Engine::start(
        config,
        key.clone(),
        operator.certify(key.id(), "storage", 0),
    )
    .await
    .expect("the storage peer starts");
    let started = Instant::now();
    let stored = |engine: &Engine| {
        engine
            .folders()
            .into_iter()
            .find_map(|f| f.encryption?.stored)
            .expect("the stored folder")
    };
    loop {
        let s = stored(&engine);
        if s.versions == files && s.missing_chunks == 0 {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3600),
            "timed out: {s:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!(
        "storage  stored {files} versions in {:.0} s; rss {} MiB",
        started.elapsed().as_secs_f64(),
        resident()
    );
    let mut longest = Duration::ZERO;
    for _ in 0..10 {
        let one = Instant::now();
        stored(&engine);
        longest = longest.max(one.elapsed());
    }
    println!(
        "storage  status longest {:.1} ms",
        longest.as_secs_f64() * 1000.0
    );
    let (before, idle) = (cpu(), Duration::from_secs(60));
    tokio::time::sleep(idle).await;
    println!(
        "storage  idle: {:.2} s of CPU in {} s; rss {} MiB",
        cpu() - before,
        idle.as_secs(),
        resident()
    );
    // Serves the second member until it has every file.
    let done = dir.join("received");
    let mut second = spawn("receive", dir, files);
    while !done.exists() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = second.wait();
    engine.shutdown().await;
}

/// The files under `dir`, counted.
fn count_files(dir: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => count_files(&e.path()),
            Ok(t) if t.is_file() => 1,
            _ => 0,
        })
        .sum()
}

async fn receive(dir: &Path, files: usize) {
    let key = second_key();
    let path = dir.join("second");
    let config = config(
        dir.join("second-data"),
        SECOND_PORT,
        vec![peer(&storage_key(), STORAGE_PORT)],
        folder(
            path.clone(),
            Encryption {
                storage_peers: vec![storage_key().id()],
                stored: None,
            },
        ),
    );
    let engine = Engine::start(config, key.clone(), user().certify(key.id(), "second", 0))
        .await
        .expect("the second member starts");
    let started = Instant::now();
    while count_files(&path) < files {
        assert!(
            started.elapsed() < Duration::from_secs(3600),
            "timed out with {} files",
            count_files(&path)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!(
        "second   received {files} files through the storage peer in {:.0} s; rss {} MiB",
        started.elapsed().as_secs_f64(),
        resident()
    );
    std::fs::write(dir.join("received"), b"").unwrap();
    engine.shutdown().await;
}

/// Runs this example as `mode` in another process.
fn spawn(mode: &str, dir: &Path, files: usize) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .arg(mode)
        .arg(dir)
        .arg(files.to_string())
        .spawn()
        .expect("the process runs")
}

fn main() {
    shoal_engine::with_allocator_settings();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") => {
            let files: usize = args[2].parse().expect("files");
            let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
            let root = dir.path().join("folder");
            for n in 0..files {
                let path = root
                    .join(format!("d{:03}", n % 1000))
                    .join(format!("f{n:07}.txt"));
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, format!("file {n}\n")).unwrap();
            }
            println!("{files} files written");
            let mut child = spawn("store", dir.path(), files);
            let runtime = tokio::runtime::Runtime::new().unwrap();
            let path = dir.path().to_owned();
            runtime.spawn(async move { member(&path).await });
            let status = child.wait().expect("the storage peer exits");
            std::process::exit(i32::from(!status.success()));
        }
        Some(mode @ ("store" | "receive")) => {
            let dir = PathBuf::from(&args[2]);
            let files: usize = args[3].parse().expect("files");
            let runtime = tokio::runtime::Runtime::new().unwrap();
            match mode {
                "store" => runtime.block_on(store(&dir, files)),
                _ => runtime.block_on(receive(&dir, files)),
            }
        }
        _ => eprintln!("usage: stored run <files>"),
    }
}
