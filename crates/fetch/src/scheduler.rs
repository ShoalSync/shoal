//! The pure scheduling policy.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::Hash;
use std::time::{Duration, Instant};

use shoal_proto::{ChunkHash, Manifest};

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
    /// Once every chunk still needed is in flight, how long one may be outstanding before it
    /// is asked of a second peer too, the first answer to come being taken: so a stalled peer
    /// does not hold up the end of a download.
    pub hedge_after: Duration,
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
            hedge_after: Duration::from_secs(2),
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
    /// Most chunks in flight to it now: [`Config::max_in_flight_per_peer`] at first, halved
    /// each time it fails or says it is busy, and one more for each chunk it sends.
    window: usize,
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
    /// The peers each chunk in flight was asked of, and when: one, or two once hedged.
    in_flight: HashMap<ChunkHash, Vec<(P, Instant)>>,
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
            window: self.config.max_in_flight_per_peer,
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

    /// Whether `hash` is still needed: false for a hedged chunk's second copy.
    pub fn wanted(&self, hash: &ChunkHash) -> bool {
        self.needed.contains_key(hash)
    }

    /// The earliest time after `now` at which a backed-off peer becomes usable again, or a
    /// chunk in flight could be hedged.
    pub fn next_wakeup(&self, now: Instant) -> Option<Instant> {
        let backed_off = self
            .peers
            .values()
            .filter(|s| !s.dropped)
            .filter_map(|s| s.available_at);
        let hedges = self
            .in_flight
            .values()
            .filter(|asked| asked.len() == 1)
            .map(|asked| asked[0].1 + self.config.hedge_after);
        backed_off.chain(hedges).filter(|t| *t > now).min()
    }

    /// Whether `state` can take one more chunk of `len` bytes.
    fn has_room(&self, state: &PeerState, len: u64) -> bool {
        state.in_flight < state.window.min(self.config.max_in_flight_per_peer)
            && (state.in_flight == 0
                || state.in_flight_bytes.saturating_add(len)
                    <= self.config.max_bytes_in_flight_per_peer)
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
            let Some(peer) = self.least_busy(&hash, len, now, None) else {
                continue;
            };
            self.assign(&peer, hash, len, now);
            planned.entry(peer).or_default().push(hash);
        }

        // The endgame: every chunk still needed is in flight. One outstanding too long is asked
        // of a second holder with room, oldest first.
        let endgame = self.needed.keys().all(|h| self.in_flight.contains_key(h));
        if endgame {
            let mut late: Vec<(Instant, ChunkHash)> = self
                .in_flight
                .iter()
                .filter(|(_, asked)| {
                    asked.len() == 1 && asked[0].1 + self.config.hedge_after <= now
                })
                .map(|(h, asked)| (asked[0].1, *h))
                .collect();
            late.sort();
            for (_, hash) in late {
                let len = self.lens.get(&hash).copied().unwrap_or(0);
                let first = self.in_flight[&hash][0].0.clone();
                let Some(peer) = self.least_busy(&hash, len, now, Some(&first)) else {
                    continue;
                };
                self.assign(&peer, hash, len, now);
                planned.entry(peer).or_default().push(hash);
            }
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

    /// The holder of `hash` with room for it and the fewest chunks in flight, other than
    /// `besides`; ties by peer order.
    fn least_busy(
        &self,
        hash: &ChunkHash,
        len: u64,
        now: Instant,
        besides: Option<&P>,
    ) -> Option<P> {
        self.peers
            .iter()
            .filter(|(p, s)| {
                Some(*p) != besides
                    && Self::usable(s, now)
                    && s.has.contains(hash)
                    && self.has_room(s, len)
            })
            .min_by(|(pa, a), (pb, b)| a.in_flight.cmp(&b.in_flight).then(pa.cmp(pb)))
            .map(|(p, _)| p.clone())
    }

    fn assign(&mut self, peer: &P, hash: ChunkHash, len: u64, now: Instant) {
        let state = self.peers.get_mut(peer).expect("chosen peer exists");
        state.in_flight += 1;
        state.in_flight_bytes += len;
        self.in_flight
            .entry(hash)
            .or_default()
            .push((peer.clone(), now));
    }

    /// Lets go of `hash` as asked of `peer`. False if it was not.
    fn release(&mut self, peer: &P, hash: &ChunkHash) -> bool {
        let Some(asked) = self.in_flight.get_mut(hash) else {
            return false;
        };
        let Some(at) = asked.iter().position(|(p, _)| p == peer) else {
            return false;
        };
        asked.remove(at);
        if asked.is_empty() {
            self.in_flight.remove(hash);
        }
        let len = self.lens.get(hash).copied().unwrap_or(0);
        if let Some(state) = self.peers.get_mut(peer) {
            state.in_flight -= 1;
            state.in_flight_bytes = state.in_flight_bytes.saturating_sub(len);
        }
        true
    }

    /// `peer` delivered `hash` (already verified).
    /// A hedged chunk's second copy, arriving after the first, only frees its peer.
    pub fn received(&mut self, peer: &P, hash: &ChunkHash) {
        if self.release(peer, hash) {
            self.needed.remove(hash);
            let max = self.config.max_in_flight_per_peer;
            if let Some(state) = self.peers.get_mut(peer) {
                state.failures = 0;
                state.window = (state.window + 1).min(max);
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
            state.window = (state.window / 2).max(1);
        }
    }

    /// A request to `peer` failed at `now`. Its chunks return to the queue and the peer backs
    /// off, or is dropped after too many consecutive failures.
    pub fn failed(&mut self, peer: &P, chunks: &[ChunkHash], now: Instant) {
        for hash in chunks {
            self.release(peer, hash);
        }
        if let Some(state) = self.peers.get_mut(peer) {
            state.window = (state.window / 2).max(1);
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
    use shoal_proto::{ChunkRef, ContentHash};

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

    #[test]
    fn a_late_chunk_is_asked_again_only_in_the_endgame() {
        let t0 = Instant::now();
        let hedge = Config::default().hedge_after;
        let mut s = Scheduler::new(&manifest(&[1, 2]), |_| false, config());
        s.set_availability(1, [h(1), h(2)]);
        s.set_availability(2, [h(1), h(2)]);
        let first = s.next_requests(t0);
        let first_of = |hash: ChunkHash| {
            first
                .iter()
                .find(|r| r.chunks.contains(&hash))
                .map(|r| r.peer)
                .unwrap()
        };
        let (one, two) = (first_of(h(1)), first_of(h(2)));
        assert_ne!(one, two, "spread over both");
        assert!(s.next_requests(t0 + hedge / 2).is_empty(), "not late yet");
        assert_eq!(s.next_wakeup(t0), Some(t0 + hedge));
        // Late: each is asked of the other peer too.
        let again = s.next_requests(t0 + hedge);
        let mut asked: Vec<(u8, ChunkHash)> = again
            .into_iter()
            .flat_map(|r| r.chunks.into_iter().map(move |c| (r.peer, c)))
            .collect();
        asked.sort();
        let mut expected = vec![(two, h(1)), (one, h(2))];
        expected.sort();
        assert_eq!(asked, expected);
        // The second answers are taken; the first, coming later, only free their peers.
        s.received(&two, &h(1));
        s.received(&one, &h(2));
        assert!(s.is_complete());
        assert!(!s.wanted(&h(1)));
        s.received(&one, &h(1));
        s.received(&two, &h(2));
        assert_eq!(s.in_flight(), 0);
        assert!(s.next_requests(t0 + hedge * 2).is_empty());
    }

    #[test]
    fn no_chunk_is_asked_twice_while_others_wait() {
        let t0 = Instant::now();
        let hedge = Config::default().hedge_after;
        let one_at_a_time = Config {
            max_in_flight_per_peer: 1,
            ..config()
        };
        let mut s = Scheduler::new(&manifest(&[1, 2]), |_| false, one_at_a_time);
        s.set_availability(1, [h(1), h(2)]);
        assert_eq!(s.next_requests(t0).len(), 1);
        // Chunk 2 waits for room, not yet in flight: no hedging, however late chunk 1 is.
        s.set_availability(2, [h(2)]);
        let later = s.next_requests(t0 + hedge * 3);
        let chunks: Vec<ChunkHash> = later.into_iter().flat_map(|r| r.chunks).collect();
        assert_eq!(chunks, [h(2)]);
    }

    #[test]
    fn a_failing_peer_is_given_fewer_chunks() {
        let t0 = Instant::now();
        let eight = Config {
            max_in_flight_per_peer: 8,
            initial_backoff: Duration::ZERO,
            ..config()
        };
        let chunks: Vec<u8> = (1..=20).collect();
        let mut s = Scheduler::new(&manifest(&chunks), |_| false, eight);
        s.set_availability(1, chunks.iter().map(|&n| h(n)));
        let asked: Vec<ChunkHash> = s
            .next_requests(t0)
            .into_iter()
            .flat_map(|r| r.chunks)
            .collect();
        assert_eq!(asked.len(), 8);
        s.failed(&1, &asked, t0);
        let asked: Vec<ChunkHash> = s
            .next_requests(t0)
            .into_iter()
            .flat_map(|r| r.chunks)
            .collect();
        assert_eq!(asked.len(), 4, "halved");
        for hash in &asked {
            s.received(&1, hash);
        }
        let more: usize = s.next_requests(t0).iter().map(|r| r.chunks.len()).sum();
        assert_eq!(more, 8, "grown back by one per chunk received");
    }
}
