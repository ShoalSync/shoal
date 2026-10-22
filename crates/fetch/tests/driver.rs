use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tidefell_fetch::{fetch, ChunkSource, Config, FetchError, Fetched};
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
    /// Requests a peer answers with one chunk and then Busy.
    busy: HashMap<&'static str, AtomicU32>,
    /// Peers that say they hold everything (ADR 0032).
    full: Vec<&'static str>,
    requests: Mutex<BTreeMap<&'static str, usize>>,
    queries: Mutex<BTreeMap<&'static str, usize>>,
    /// Requests to each peer under way now, and the most at once.
    outstanding: Mutex<BTreeMap<&'static str, (usize, usize)>>,
}

impl ChunkSource for Sim {
    type Peer = &'static str;

    async fn availability(
        &self,
        peer: &&'static str,
        _: &Manifest,
    ) -> Result<Vec<ChunkHash>, String> {
        *self.queries.lock().unwrap().entry(peer).or_default() += 1;
        Ok(self.holdings.get(peer).cloned().unwrap_or_default())
    }

    fn holds_everything(&self, peer: &&'static str) -> bool {
        self.full.contains(peer)
    }

    async fn fetch(&self, peer: &&'static str, chunks: &[ChunkHash]) -> Result<Fetched, String> {
        *self.requests.lock().unwrap().entry(peer).or_default() += 1;
        {
            let mut outstanding = self.outstanding.lock().unwrap();
            let (now, most) = outstanding.entry(peer).or_default();
            *now += 1;
            *most = (*most).max(*now);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.outstanding.lock().unwrap().get_mut(peer).unwrap().0 -= 1;
        if let Some(left) = self.failures.get(peer) {
            if left.load(Ordering::SeqCst) > 0 {
                left.fetch_sub(1, Ordering::SeqCst);
                return Err("connection reset".into());
            }
        }
        if self.liars.contains(peer) {
            return Ok(Fetched::default());
        }
        let held = &self.holdings[peer];
        let mut sent: Vec<(ChunkHash, Vec<u8>)> = chunks
            .iter()
            .filter(|c| held.contains(c))
            .map(|c| (*c, c.0[..1].to_vec()))
            .collect();
        if let Some(left) = self.busy.get(peer) {
            if left.load(Ordering::SeqCst) > 0 {
                left.fetch_sub(1, Ordering::SeqCst);
                sent.truncate(1);
                return Ok(Fetched {
                    chunks: sent,
                    busy_for: Some(Duration::from_secs(1)),
                });
            }
        }
        Ok(Fetched {
            chunks: sent,
            busy_for: None,
        })
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

#[tokio::test(start_paused = true)]
async fn a_busy_peer_is_asked_again_later_not_dropped() {
    // More Busy answers than the failures that would drop a peer.
    let mut sim = Sim::default();
    sim.holdings.insert("a", (1..=10).map(h).collect());
    sim.busy.insert("a", AtomicU32::new(6));
    let sim = Arc::new(sim);
    let mut got = Vec::new();
    let start = tokio::time::Instant::now();
    let report = fetch(
        sim.clone(),
        &manifest(10),
        &["a"],
        |_| false,
        Config::default(),
        |hash, _| {
            got.push(hash);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(report.chunks, 10);
    got.sort();
    assert_eq!(got, (1..=10).map(h).collect::<Vec<_>>());
    // Busy answers made the fetcher wait before asking again, instead of failing.
    assert!(start.elapsed() >= Duration::from_secs(2));
}

fn queries(sim: &Sim, peer: &'static str) -> usize {
    sim.queries.lock().unwrap().get(peer).copied().unwrap_or(0)
}

#[tokio::test(start_paused = true)]
async fn a_lone_peer_is_not_asked_what_it_holds() {
    let mut sim = Sim::default();
    sim.holdings
        .insert("a", manifest(4).chunks.iter().map(|c| c.hash).collect());
    let sim = Arc::new(sim);
    let report = fetch(
        sim.clone(),
        &manifest(4),
        &["a"],
        |_| false,
        Config::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(report.chunks, 4);
    assert_eq!(queries(&sim, "a"), 0);
}

#[tokio::test(start_paused = true)]
async fn a_full_replica_is_not_asked_what_it_holds() {
    let mut sim = Sim::default();
    let all: Vec<ChunkHash> = manifest(4).chunks.iter().map(|c| c.hash).collect();
    sim.holdings.insert("full", all.clone());
    sim.holdings.insert("partial", all[..2].to_vec());
    sim.full.push("full");
    let sim = Arc::new(sim);
    let report = fetch(
        sim.clone(),
        &manifest(4),
        &["full", "partial"],
        |_| false,
        Config::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(report.chunks, 4);
    assert_eq!(queries(&sim, "full"), 0);
    assert_eq!(queries(&sim, "partial"), 1);
}

#[tokio::test(start_paused = true)]
async fn a_full_replica_short_of_a_chunk_does_not_stop_the_fetch() {
    let mut sim = Sim::default();
    let all: Vec<ChunkHash> = manifest(4).chunks.iter().map(|c| c.hash).collect();
    // It says it is a full replica, but has not got chunk 4 yet; another peer has.
    sim.holdings.insert("full", all[..3].to_vec());
    sim.holdings.insert("other", all[3..].to_vec());
    sim.full.push("full");
    let sim = Arc::new(sim);
    let report = fetch(
        sim.clone(),
        &manifest(4),
        &["full", "other"],
        |_| false,
        Config::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(report.chunks, 4);
    assert_eq!(report.per_peer[&"full"], 3);
    assert_eq!(report.per_peer[&"other"], 1);
}

#[tokio::test(start_paused = true)]
async fn a_lone_peer_without_the_chunks_leaves_them_unobtainable() {
    let mut sim = Sim::default();
    sim.holdings.insert("a", vec![h(1)]);
    let sim = Arc::new(sim);
    let result = fetch(
        sim.clone(),
        &manifest(3),
        &["a"],
        |_| false,
        Config::default(),
        |_, _| Ok(()),
    )
    .await;
    assert!(
        matches!(result, Err(FetchError::Unobtainable(ref c)) if c.len() == 2),
        "{result:?}"
    );
    assert_eq!(queries(&sim, "a"), 1);
}

/// A large download keeps several requests to a peer under way at once, so a round trip is
/// waited out once per many requests (ADR 0032).
#[tokio::test(start_paused = true)]
async fn requests_to_a_peer_overlap() {
    let mut sim = Sim::default();
    sim.holdings.insert("a", (1..=40).map(h).collect());
    let sim = Arc::new(sim);
    let report = fetch(
        sim.clone(),
        &manifest(40),
        &["a"],
        |_| false,
        Config::default(),
        |_, _| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(report.chunks, 40);
    let most = sim.outstanding.lock().unwrap()["a"].1;
    assert!(most >= 5, "at most {most} requests at once");
}
