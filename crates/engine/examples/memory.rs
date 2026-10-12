//! Measures the engine's memory on a synthetic folder (ADR 0035).
//!
//! `cargo run --release -p tidefell-engine --example memory -- run <files> <bytes>` generates a
//! folder of `files` sparse files, `bytes` in all, then measures a separate engine process on it:
//! resident memory after it opens the folder, after a full scan, after repeated status reads (and
//! the longest of those), and idle. Linux only.
//!
//! A third argument, `run <files> <bytes> <MiB>`, is a target: the run fails if the engine's
//! resident memory, once it has opened and scanned the folder and is idle, is above it. The
//! peak while it opens is reported but not held to it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use tidefell_engine::synth::{generate, SynthFolder};
use tidefell_engine::{Engine, EngineConfig, FolderConfig, RelayConfig, ReplicaMode, Retention};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

#[global_allocator]
static ALLOCATOR: tidefell_engine::Allocator = tidefell_engine::Allocator;

const FOLDER: FolderId = FolderId([42; 32]);

fn keys() -> (UserKey, DeviceKey) {
    (UserKey::from_seed([1; 32]), DeviceKey::from_seed([2; 32]))
}

fn folder(dir: &Path, user: &UserKey) -> FolderConfig {
    FolderConfig {
        id: FOLDER,
        path: dir.join("folder"),
        mode: ReplicaMode::Full,
        members: vec![user.id()],
        retention: Retention::Days(30),
        scan_interval: Duration::from_secs(3600),
        sync: Default::default(),
        confirm_deletes: Default::default(),
    }
}

/// Resident and peak resident memory of this process, in MiB.
fn memory() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .map_or(0, |kb| kb / 1024)
    };
    (field("VmRSS:"), field("VmHWM:"))
}

/// Memory freed to the system lazily (`MADV_FREE`), in MiB: still counted as resident, but the
/// system takes it back as soon as it needs it.
fn lazy_free() -> u64 {
    std::fs::read_to_string("/proc/self/smaps_rollup")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("LazyFree:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map_or(0, |kb| kb / 1024)
}

fn report(phase: &str, started: Instant) {
    let (rss, peak) = memory();
    println!(
        "{phase:<8} rss {rss:>6} MiB  peak {peak:>6} MiB  lazily freed {:>5} MiB  {:>7.1} s",
        lazy_free(),
        started.elapsed().as_secs_f64()
    );
}

/// Measures the engine on the folder in `dir`; returns its resident memory when idle, in MiB.
async fn measure(dir: &Path) -> u64 {
    let (user, key) = keys();
    let config = EngineConfig {
        data_dir: dir.join("data"),
        metadata_dir: None,
        relay: RelayConfig::Disabled,
        address_lookup: false,
        local_discovery: false,
        watch: true,
        limits: Default::default(),
        bind: Some(std::net::SocketAddr::from(([127, 0, 0, 1], 0))),
        peers: Vec::new(),
        folders: vec![folder(dir, &user)],
        min_free_space: None,
    };
    let started = Instant::now();
    let engine = Engine::start(config, key.clone(), user.certify(key.id(), "measure", 0))
        .await
        .expect("the engine starts");
    report("open", started);
    let started = Instant::now();
    engine.scan(FOLDER).await.expect("the folder scans");
    report("scan", started);
    let started = Instant::now();
    let mut longest = Duration::ZERO;
    for _ in 0..10 {
        let one = Instant::now();
        let folders = engine.folders();
        assert_eq!(folders.len(), 1);
        longest = longest.max(one.elapsed());
    }
    report("status", started);
    println!("status   longest {:.1} ms", longest.as_secs_f64() * 1000.0);
    tokio::time::sleep(Duration::from_secs(5)).await;
    report("idle", Instant::now());
    let (idle, _) = memory();
    engine.shutdown().await;
    idle
}

fn main() {
    // Measured as the engine runs (ADR 0035).
    tidefell_engine::with_allocator_settings();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("run") => {
            let files: usize = args[2].parse().expect("files");
            let bytes: u64 = args[3].parse().expect("bytes");
            let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
            let (user, key) = keys();
            let started = Instant::now();
            generate(
                &dir.path().join("data"),
                &folder(dir.path(), &user),
                &key,
                SynthFolder {
                    files,
                    total_bytes: bytes,
                },
            )
            .expect("the folder generates");
            println!(
                "{files} files, {} GiB, generated in {:.0} s",
                bytes >> 30,
                started.elapsed().as_secs_f64()
            );
            let mut measure = Command::new(std::env::current_exe().unwrap());
            measure.arg("measure").arg(dir.path());
            if let Some(target) = args.get(4) {
                measure.arg(target);
            }
            let status = measure.status().expect("the measurement runs");
            if !status.success() {
                std::process::exit(1);
            }
        }
        Some("generate") => {
            let dir = PathBuf::from(&args[2]);
            let (user, key) = keys();
            generate(
                &dir.join("data"),
                &folder(&dir, &user),
                &key,
                SynthFolder {
                    files: args[3].parse().expect("files"),
                    total_bytes: args[4].parse().expect("bytes"),
                },
            )
            .expect("the folder generates");
        }
        Some("measure") => {
            let dir = PathBuf::from(&args[2]);
            let idle = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(measure(&dir));
            if let Some(target) = args.get(3) {
                let target: u64 = target.parse().expect("target in MiB");
                if idle > target {
                    println!("over the target: {idle} MiB idle, above {target} MiB");
                    std::process::exit(1);
                }
                println!("within the target of {target} MiB");
            }
        }
        _ => eprintln!(
            "usage: memory run <files> <bytes> [MiB] | generate <dir> <files> <bytes> | measure <dir> [MiB]"
        ),
    }
}
