use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tidefell_fetch::{fetch, ChunkSource, Config, FetchError};
use tidefell_proto::{ChunkHash, ChunkRef, ContentHash, Manifest};

fn h(n: u8) -> ChunkHash {
    ChunkHash([n; 32])
}

fn manifest(n: u8) -> Manifest {
    Manifest {
        content: ContentHash([0; 32]),
        chunks: (1..=n).map(|i| ChunkRef { hash: h(i), len: 1 }).collect(),
    }
}

#[derive(Default)]
struct Sim {
    holdings: HashMap<&'static str, Vec<ChunkHash>>,
    /// Requests that fail before a peer starts answering.
    failures: HashMap<&'static str, AtomicU32>,
    /// Peers that claim chunks but answer Missing.
    liars: Vec<&'static str>,
    requests: Mutex<BTreeMap<&'static str, usize>>,
}

impl ChunkSource for Sim {
    type Peer = &'static str;

    async fn availability(
        &self,
        peer: &&'static str,
        _: &Manifest,
    ) -> Result<Vec<ChunkHash>, String> {
        Ok(self.holdings.get(peer).cloned().unwrap_or_default())
    }

    async fn fetch(
        &self,
        peer: &&'static str,
        chunks: &[ChunkHash],
    ) -> Result<Vec<(ChunkHash, Vec<u8>)>, String> {
        *self.requests.lock().unwrap().entry(peer).or_default() += 1;
        tokio::time::sleep(Duration::from_millis(10)).await;
        if let Some(left) = self.failures.get(peer) {
            if left.load(Ordering::SeqCst) > 0 {
                left.fetch_sub(1, Ordering::SeqCst);
                return Err("connection reset".into());
            }
        }
        if self.liars.contains(peer) {
            return Ok(Vec::new());
        }
        let held = &self.holdings[peer];
        Ok(chunks
            .iter()
            .filter(|c| held.contains(c))
            .map(|c| (*c, c.0[..1].to_vec()))
            .collect())
    }
}

#[tokio::test(start_paused = true)]
async fn fetches_from_several_peers() {
    let mut sim = Sim::default();
    sim.holdings.insert("a", (1..=20).map(h).collect());
    sim.holdings.insert("b", (1..=20).map(h).collect());
    let sim = Arc::new(sim);
    let mut got = Vec::new();
    let report = fetch(
        sim.clone(),
        &manifest(20),
        &["a", "b"],
        |c| *c == h(20),
        Config::default(),
        |hash, _| {
            got.push(hash);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(report.chunks, 19);
    assert_eq!(got.len(), 19);
    assert!(
        report.per_peer["a"] > 0 && report.per_peer["b"] > 0,
        "load is spread"
    );
}

#[tokio::test(start_paused = true)]
async fn rare_chunk_only_from_its_holder() {
    let mut sim = Sim::default();
    sim.holdings.insert("common", (1..=3).map(h).collect());
    sim.holdings.insert("rare", vec![h(4)]);
    let report = fetch(
        Arc::new(sim),
        &manifest(4),
        &["common", "rare"],
        |_| false,
        Config::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(report.per_peer["rare"], 1);
    assert_eq!(report.per_peer["common"], 3);
}

#[tokio::test(start_paused = true)]
async fn retries_after_transient_failures() {
    let mut sim = Sim::default();
    sim.holdings.insert("flaky", (1..=4).map(h).collect());
    sim.failures.insert("flaky", AtomicU32::new(2));
    let report = fetch(
        Arc::new(sim),
        &manifest(4),
        &["flaky"],
        |_| false,
        Config::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(report.chunks, 4);
}

#[tokio::test(start_paused = true)]
async fn falls_back_when_a_peer_lies_or_dies() {
    let mut sim = Sim::default();
    sim.holdings.insert("liar", (1..=4).map(h).collect());
    sim.liars.push("liar");
    sim.holdings.insert("dead", (1..=4).map(h).collect());
    sim.failures.insert("dead", AtomicU32::new(u32::MAX));
    sim.holdings.insert("good", (1..=4).map(h).collect());
    let report = fetch(
        Arc::new(sim),
        &manifest(4),
        &["liar", "dead", "good"],
        |_| false,
        Config::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(report.per_peer.get("good"), Some(&4));
}

#[tokio::test(start_paused = true)]
async fn reports_unobtainable_chunks() {
    let mut sim = Sim::default();
    sim.holdings.insert("a", vec![h(1)]);
    sim.holdings.insert("dead", vec![h(2)]);
    sim.failures.insert("dead", AtomicU32::new(u32::MAX));
    let err = fetch(
        Arc::new(sim),
        &manifest(3),
        &["a", "dead"],
        |_| false,
        Config::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap_err();
    let FetchError::Unobtainable(mut missing) = err else {
        panic!("wrong error: {err:?}");
    };
    missing.sort();
    assert_eq!(missing, vec![h(2), h(3)]);
}

#[tokio::test(start_paused = true)]
async fn nothing_to_do_when_complete() {
    let report = fetch(
        Arc::new(Sim::default()),
        &manifest(3),
        &[],
        |_| true,
        Config::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(report.chunks, 0);
}
