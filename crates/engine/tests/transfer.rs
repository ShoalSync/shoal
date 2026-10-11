//! Transfer efficiency (ADR 0032): what syncing many small files and one large file costs in
//! requests and time, with a fixed delay before every request standing in for a slow link's
//! round trips.
//!
//! The measurement is slow, so it only runs when asked:
//!
//! ```text
//! cargo test --release -p tidefell-engine --test transfer -- --ignored --nocapture
//! ```
//!
//! `TIDEFELL_REQUEST_DELAY_MS` sets the delay (100 by default).

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tidefell_engine::{
    Engine, EngineConfig, FolderConfig, FolderStatus, RelayConfig, ReplicaMode, Retention,
};
use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::FolderId;

const FOLDER: FolderId = FolderId([32; 32]);

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
                // Scans are triggered explicitly so only the measured files move.
                scan_interval: Duration::from_secs(3600),
                sync: Default::default(),
            }],
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
}

async fn eventually(what: &str, timeout: Duration, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn status(engine: &Engine) -> FolderStatus {
    engine.status(FOLDER).unwrap()
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// The request counters in an engine's metrics.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Requests {
    manifest: f64,
    availability: f64,
    replica: f64,
    chunks: f64,
    chunks_requested: f64,
}

impl Requests {
    fn of(engine: &Engine) -> Self {
        let samples: HashMap<String, f64> = engine
            .metrics()
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| {
                let (name, value) = l.rsplit_once(' ')?;
                Some((name.to_string(), value.parse().ok()?))
            })
            .collect();
        let get = |name: &str| samples.get(name).copied().unwrap_or(0.0);
        Self {
            manifest: get(r#"tidefell_requests_total{kind="manifest"}"#),
            availability: get(r#"tidefell_requests_total{kind="availability"}"#),
            replica: get(r#"tidefell_requests_total{kind="replica"}"#),
            chunks: get(r#"tidefell_requests_total{kind="chunks"}"#),
            chunks_requested: get("tidefell_chunks_requested_total"),
        }
    }

    fn since(self, before: Self) -> Self {
        Self {
            manifest: self.manifest - before.manifest,
            availability: self.availability - before.availability,
            replica: self.replica - before.replica,
            chunks: self.chunks - before.chunks,
            chunks_requested: self.chunks_requested - before.chunks_requested,
        }
    }

    /// Every request opens a stream; replica queries run on their own timer, not per file.
    fn streams(self) -> f64 {
        self.manifest + self.availability + self.chunks
    }
}

fn report(case: &str, files: usize, elapsed: Duration, r: Requests) {
    let n = files as f64;
    println!(
        "{case}: {files} files in {:.1} s; per file: {:.2} streams, {:.2} manifest, \
         {:.2} availability, {:.2} chunk requests, {:.2} chunks; {} replica queries",
        elapsed.as_secs_f64(),
        r.streams() / n,
        r.manifest / n,
        r.availability / n,
        r.chunks / n,
        r.chunks_requested / n,
        r.replica,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    for i in 0..5 {
        a.write(&format!("small/{i}.bin"), &noise(2048, 2 * i + 1));
    }
    let engine_a = a.start(&user).await;
    let engine_b = b.start(&user).await;
    engine_a.scan(FOLDER).await.unwrap();
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("files on b", Duration::from_secs(20), || {
        (0..5).all(|i| b.read(&format!("small/{i}.bin")) == Some(noise(2048, 2 * i + 1)))
    })
    .await;

    let r = Requests::of(&engine_b);
    assert!(r.chunks >= 1.0 && r.chunks_requested >= 5.0, "{r:?}");
    // A file smaller than a chunk is fetched without its manifest.
    assert_eq!(r.manifest, 0.0, "{r:?}");
    assert_eq!(Requests::of(&engine_a).chunks, 0.0);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement of several minutes; run it with --ignored"]
async fn measure_small_files_and_a_large_one() {
    const SMALL: usize = 1000;
    const LARGE: usize = 256 * 1024 * 1024;
    let delay = std::env::var("TIDEFELL_REQUEST_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(Duration::from_millis(100), Duration::from_millis);
    let timeout = Duration::from_secs(3600);

    let dir = tempfile::tempdir().unwrap();
    let user = UserKey::from_seed([1; 32]);
    let a = Replica::new(dir.path(), "a", 10);
    let b = Replica::new(dir.path(), "b", 11);
    let small = |i: usize| noise(2048, 2 * i as u64 + 1);
    for i in 0..SMALL {
        a.write(&format!("small/{i:04}.bin"), &small(i));
    }
    let engine_a = a.start(&user).await;
    let engine_b = b.start(&user).await;
    engine_a.set_request_delay(delay);
    engine_b.set_request_delay(delay);
    engine_a.scan(FOLDER).await.unwrap();
    println!("request delay {} ms", delay.as_millis());

    let start = Instant::now();
    let before = Requests::of(&engine_b);
    engine_b.connect(engine_a.addr()).await.unwrap();
    eventually("small files on b", timeout, || {
        status(&engine_b).files.len() == SMALL && status(&engine_b).pending == 0
    })
    .await;
    report(
        "small files",
        SMALL,
        start.elapsed(),
        Requests::of(&engine_b).since(before),
    );
    for i in 0..SMALL {
        let path = format!("small/{i:04}.bin");
        assert_eq!(b.read(&path), Some(small(i)), "{path}");
    }

    let large = noise(LARGE, 99);
    a.write("large.bin", &large);
    let start = Instant::now();
    let before = Requests::of(&engine_b);
    engine_a.scan(FOLDER).await.unwrap();
    eventually("large file on b", timeout, || {
        status(&engine_b).files.len() == SMALL + 1 && status(&engine_b).pending == 0
    })
    .await;
    report(
        "large file",
        1,
        start.elapsed(),
        Requests::of(&engine_b).since(before),
    );
    assert!(b.read("large.bin") == Some(large), "large.bin differs");
}
