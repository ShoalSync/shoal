//! The pure scheduling policy.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::Hash;
use std::time::{Duration, Instant};

use tidefell_proto::{ChunkHash, Manifest};

/// Scheduler limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// Most chunks in flight to one peer at a time. Enough to keep several requests to it
    /// outstanding at once, so a round trip is waited out once per many requests rather than
    /// once per request (ADR 0032).
    pub max_in_flight_per_peer: usize,
    /// Most bytes of chunks in flight to one peer at a time, by their lengths in the manifest;
    /// a single chunk larger than this still goes on its own.
    pub max_bytes_in_flight_per_peer: u64,
    /// Most chunks in one request (blobs/1 allows 64).
    pub max_chunks_per_request: usize,
    /// First backoff after a failure; doubles per consecutive failure.
    pub initial_backoff: Duration,
    /// Longest backoff.
    pub max_backoff: Duration,
    /// Consecutive failures after which a peer is dropped.
    pub max_failures: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_in_flight_per_peer: 64,
            max_bytes_in_flight_per_peer: 16 << 20,
            max_chunks_per_request: 8,
            initial_backoff: Duration::from_millis(200),
            max_backoff: Duration::from_secs(10),
            max_failures: 5,
        }
    }
}

/// Chunks to request from one peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<P> {
    /// The peer.
    pub peer: P,
    /// The chunks, by hash.
    pub chunks: Vec<ChunkHash>,
}

#[derive(Debug)]
struct PeerState {
    /// Chunk hashes this peer is believed to hold.
    has: BTreeSet<ChunkHash>,
    in_flight: usize,
    in_flight_bytes: u64,
    failures: u32,
    available_at: Option<Instant>,
    dropped: bool,
}

/// Decides which chunks to request from which peers.
#[derive(Debug)]
pub struct Scheduler<P> {
    config: Config,
    /// Distinct chunks still needed, with their position of first appearance (for stable order).
    needed: BTreeMap<ChunkHash, usize>,
    /// Each needed chunk's length.
    lens: HashMap<ChunkHash, u64>,
    in_flight: HashMap<ChunkHash, P>,
    peers: HashMap<P, PeerState>,
}

impl<P: Clone + Eq + Hash + Ord> Scheduler<P> {
    /// Schedules the chunks of `manifest` for which `have` returns false.
    pub fn new(manifest: &Manifest, have: impl Fn(&ChunkHash) -> bool, config: Config) -> Self {
        let mut needed = BTreeMap::new();
        let mut lens = HashMap::new();
        for (i, chunk) in manifest.chunks.iter().enumerate() {
            if !have(&chunk.hash) {
                needed.entry(chunk.hash).or_insert(i);
                lens.insert(chunk.hash, u64::from(chunk.len));
            }
        }
        Self {
            config,
            needed,
            lens,
            in_flight: HashMap::new(),
            peers: HashMap::new(),
        }
    }

    /// Records which chunks `peer` holds, replacing earlier knowledge.
    pub fn set_availability(&mut self, peer: P, has: impl IntoIterator<Item = ChunkHash>) {
        let state = self.peers.entry(peer).or_insert(PeerState {
            has: BTreeSet::new(),
            in_flight: 0,
            in_flight_bytes: 0,
            failures: 0,
            available_at: None,
            dropped: false,
        });
        state.has = has.into_iter().collect();
    }

    /// True when every needed chunk has arrived.
    pub fn is_complete(&self) -> bool {
        self.needed.is_empty()
    }

    /// Chunks still needed.
    pub fn remaining(&self) -> impl Iterator<Item = &ChunkHash> {
        self.needed.keys()
    }

    /// Chunks in flight.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    fn usable(state: &PeerState, now: Instant) -> bool {
        !state.dropped && state.available_at.is_none_or(|t| t <= now)
    }

    /// Needed chunks that no remaining peer holds. When nothing is in flight and this equals
    /// the remaining set, the fetch cannot finish with the current peers.
    pub fn unobtainable(&self) -> Vec<ChunkHash> {
        self.needed
            .keys()
            .filter(|h| {
                !self.in_flight.contains_key(*h)
                    && !self
                        .peers
                        .values()
                        .any(|s| !s.dropped && s.has.contains(*h))
            })
            .copied()
            .collect()
    }

    /// The earliest time after `now` at which a backed-off peer becomes usable again.
    pub fn next_wakeup(&self, now: Instant) -> Option<Instant> {
        self.peers
            .values()
            .filter(|s| !s.dropped)
            .filter_map(|s| s.available_at)
            .filter(|t| *t > now)
            .min()
    }

    /// Plans new requests at time `now`, marking their chunks in flight.
    pub fn next_requests(&mut self, now: Instant) -> Vec<Request<P>> {
        // Rarity of each schedulable chunk among usable peers.
        let mut candidates: Vec<(usize, usize, ChunkHash)> = self
            .needed
            .iter()
            .filter(|(h, _)| !self.in_flight.contains_key(*h))
            .filter_map(|(h, &pos)| {
                let holders = self
                    .peers
                    .values()
                    .filter(|s| Self::usable(s, now) && s.has.contains(h))
                    .count();
                (holders > 0).then_some((holders, pos, *h))
            })
            .collect();
        candidates.sort();

        let mut planned: BTreeMap<P, Vec<ChunkHash>> = BTreeMap::new();
        for (_, _, hash) in candidates {
            let len = self.lens.get(&hash).copied().unwrap_or(0);
            // The holder with capacity and the fewest chunks in flight; ties by peer order.
            let choice = self
                .peers
                .iter()
                .filter(|(_, s)| {
                    Self::usable(s, now)
                        && s.has.contains(&hash)
                        && s.in_flight < self.config.max_in_flight_per_peer
                        && (s.in_flight == 0
                            || s.in_flight_bytes.saturating_add(len)
                                <= self.config.max_bytes_in_flight_per_peer)
                })
                .min_by(|(pa, a), (pb, b)| a.in_flight.cmp(&b.in_flight).then(pa.cmp(pb)))
                .map(|(p, _)| p.clone());
            let Some(peer) = choice else { continue };
            let state = self.peers.get_mut(&peer).expect("chosen peer exists");
            state.in_flight += 1;
            state.in_flight_bytes += len;
            self.in_flight.insert(hash, peer.clone());
            planned.entry(peer).or_default().push(hash);
        }

        planned
            .into_iter()
            .flat_map(|(peer, chunks)| {
                chunks
                    .chunks(self.config.max_chunks_per_request)
                    .map(|c| Request {
                        peer: peer.clone(),
                        chunks: c.to_vec(),
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn release(&mut self, peer: &P, hash: &ChunkHash) -> bool {
        if self.in_flight.get(hash) != Some(peer) {
            return false;
        }
        self.in_flight.remove(hash);
        let len = self.lens.get(hash).copied().unwrap_or(0);
        if let Some(state) = self.peers.get_mut(peer) {
            state.in_flight -= 1;
            state.in_flight_bytes = state.in_flight_bytes.saturating_sub(len);
        }
        true
    }

    /// `peer` delivered `hash` (already verified).
    pub fn received(&mut self, peer: &P, hash: &ChunkHash) {
        if self.release(peer, hash) {
            self.needed.remove(hash);
            if let Some(state) = self.peers.get_mut(peer) {
                state.failures = 0;
            }
        }
    }

    /// `peer` does not have `hash` after all.
    pub fn missing(&mut self, peer: &P, hash: &ChunkHash) {
        if self.release(peer, hash) {
            if let Some(state) = self.peers.get_mut(peer) {
                state.has.remove(hash);
            }
        }
    }

    /// `peer` was at its upload limit and did not send `chunks` (ADR 0029). They return to the
    /// queue, and the peer is asked again no sooner than `until`, without counting a failure.
    pub fn busy(&mut self, peer: &P, chunks: &[ChunkHash], until: Instant) {
        for hash in chunks {
            self.release(peer, hash);
        }
        if let Some(state) = self.peers.get_mut(peer) {
            state.available_at = Some(state.available_at.map_or(until, |at| at.max(until)));
        }
    }

    /// A request to `peer` failed at `now`. Its chunks return to the queue and the peer backs
    /// off, or is dropped after too many consecutive failures.
    pub fn failed(&mut self, peer: &P, chunks: &[ChunkHash], now: Instant) {
        for hash in chunks {
            self.release(peer, hash);
        }
        if let Some(state) = self.peers.get_mut(peer) {
            state.failures += 1;
            if state.failures >= self.config.max_failures {
                state.dropped = true;
                return;
            }
            let factor = 1u32 << (state.failures - 1).min(16);
            let backoff = self
                .config
                .initial_backoff
                .saturating_mul(factor)
                .min(self.config.max_backoff);
            state.available_at = Some(now + backoff);
        }
    }
}

#[cfg(test)]
mod tests {
    use tidefell_proto::{ChunkRef, ContentHash};

    use super::*;

    fn h(n: u8) -> ChunkHash {
        ChunkHash([n; 32])
    }

    fn manifest(chunks: &[u8]) -> Manifest {
        Manifest {
            content: ContentHash([0; 32]),
            chunks: chunks
                .iter()
                .map(|&n| ChunkRef { hash: h(n), len: 1 })
                .collect(),
        }
    }

    fn config() -> Config {
        Config {
            max_in_flight_per_peer: 2,
            max_chunks_per_request: 8,
            ..Config::default()
        }
    }

    #[test]
    fn skips_local_chunks_and_duplicates() {
        let s: Scheduler<u8> = Scheduler::new(&manifest(&[1, 2, 2, 3]), |c| *c == h(3), config());
        assert_eq!(s.remaining().copied().collect::<Vec<_>>(), vec![h(1), h(2)]);
    }

    #[test]
    fn rarest_first_and_load_spreading() {
        let now = Instant::now();
        let mut s = Scheduler::new(&manifest(&[1, 2, 3, 4]), |_| false, config());
        // Chunk 4 is only on peer 2; the rest are on both.
        s.set_availability(1, [h(1), h(2), h(3)]);
        s.set_availability(2, [h(1), h(2), h(3), h(4)]);

        let requests = s.next_requests(now);
        let for_peer = |p| {
            requests
                .iter()
                .filter(|r| r.peer == p)
                .flat_map(|r| r.chunks.clone())
                .collect::<Vec<_>>()
        };
        // The rare chunk goes to its only holder first; the rest fill capacity evenly.
        assert_eq!(for_peer(2)[0], h(4));
        assert_eq!(for_peer(1).len(), 2);
        assert_eq!(for_peer(2).len(), 2);
        assert_eq!(s.in_flight(), 4);
        assert!(
            s.next_requests(now).is_empty(),
            "everything already in flight"
        );
    }

    #[test]
    fn per_peer_limit() {
        let now = Instant::now();
        let mut s = Scheduler::new(&manifest(&[1, 2, 3, 4, 5]), |_| false, config());
        s.set_availability(1, [h(1), h(2), h(3), h(4), h(5)]);
        assert_eq!(s.next_requests(now)[0].chunks.len(), 2);
        s.received(&1, &h(1));
        assert_eq!(s.next_requests(now)[0].chunks, vec![h(3)]);
    }

    #[test]
    fn missing_chunk_moves_to_another_holder() {
        let now = Instant::now();
        let mut s = Scheduler::new(&manifest(&[1]), |_| false, config());
        s.set_availability(1, [h(1)]);
        s.set_availability(2, [h(1)]);
        let first = s.next_requests(now);
        assert_eq!(first[0].peer, 1);
        s.missing(&1, &h(1));
        let second = s.next_requests(now);
        assert_eq!(second[0].peer, 2);
        s.received(&2, &h(1));
        assert!(s.is_complete());
    }

    #[test]
    fn failures_back_off_then_drop() {
        let t0 = Instant::now();
        let mut s = Scheduler::new(&manifest(&[1]), |_| false, config());
        s.set_availability(1, [h(1)]);

        let mut at = t0;
        for attempt in 1..=4u32 {
            let requests = s.next_requests(at);
            assert_eq!(requests.len(), 1, "attempt {attempt}");
            s.failed(&1, &requests[0].chunks, at);
            let wake = s.next_wakeup(at).unwrap();
            let expected = config().initial_backoff * (1 << (attempt - 1));
            assert_eq!(wake - at, expected);
            assert!(s.next_requests(at).is_empty(), "backed off");
            assert!(
                s.next_wakeup(wake).is_none(),
                "no wakeup once the backoff has passed"
            );
            at = wake;
        }
        let requests = s.next_requests(at);
        s.failed(&1, &requests[0].chunks, at);
        assert_eq!(
            s.unobtainable(),
            vec![h(1)],
            "peer dropped after max failures"
        );
        assert!(s.next_wakeup(at).is_none());
    }

    #[test]
    fn unobtainable_when_nobody_has_it() {
        let mut s = Scheduler::new(&manifest(&[1, 2]), |_| false, config());
        s.set_availability(1, [h(1)]);
        assert_eq!(s.unobtainable(), vec![h(2)]);
    }

    #[test]
    fn bytes_in_flight_are_bounded() {
        let now = Instant::now();
        let manifest = Manifest {
            content: ContentHash([0; 32]),
            chunks: (1..=4)
                .map(|n| ChunkRef {
                    hash: h(n),
                    len: 6 << 20,
                })
                .collect(),
        };
        let mut s = Scheduler::new(&manifest, |_| false, Config::default());
        s.set_availability(1, (1..=4).map(h));
        // Two 6 MiB chunks fit in 16 MiB; a third would not.
        let first: usize = s.next_requests(now).iter().map(|r| r.chunks.len()).sum();
        assert_eq!(first, 2);
        s.received(&1, &h(1));
        let second: usize = s.next_requests(now).iter().map(|r| r.chunks.len()).sum();
        assert_eq!(second, 1);

        // A chunk larger than the bound still goes, on its own.
        let huge = Manifest {
            content: ContentHash([0; 32]),
            chunks: vec![ChunkRef {
                hash: h(9),
                len: 64 << 20,
            }],
        };
        let mut s = Scheduler::new(&huge, |_| false, Config::default());
        s.set_availability(1, [h(9)]);
        assert_eq!(s.next_requests(now).len(), 1);
    }
}
