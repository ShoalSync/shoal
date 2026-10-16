//! File content sent and received, and requests made of peers, for status displays and metrics
//! (ADR 0030, ADR 0032).

use crate::locks::Locked;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

/// Seconds the rate is averaged over.
const WINDOW: u64 = 10;

/// Bytes moved in one direction: a total and the last few seconds, one bucket per second.
#[derive(Debug)]
struct Counter {
    total: AtomicU64,
    /// (second since start, bytes in it), oldest first.
    recent: Mutex<VecDeque<(u64, u64)>>,
}

impl Counter {
    fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            recent: Mutex::new(VecDeque::new()),
        }
    }

    fn add(&self, second: u64, bytes: u64) {
        self.total.fetch_add(bytes, Ordering::Relaxed);
        let mut recent = self.recent.locked();
        match recent.back_mut() {
            Some((s, b)) if *s == second => *b = b.saturating_add(bytes),
            _ => recent.push_back((second, bytes)),
        }
        while recent
            .front()
            .is_some_and(|(s, _)| s.saturating_add(WINDOW) < second)
        {
            recent.pop_front();
        }
    }

    /// Bytes per second over the last full seconds, not counting the one under way.
    fn rate(&self, second: u64) -> u64 {
        let recent = self.recent.locked();
        let from = second.saturating_sub(WINDOW);
        let bytes: u64 = recent
            .iter()
            .filter(|(s, _)| *s >= from && *s < second)
            .map(|(_, b)| b)
            .sum();
        bytes / WINDOW
    }
}

/// File content sent to and received from peers since the node started.
#[derive(Debug)]
pub(crate) struct Traffic {
    start: Instant,
    sent: Counter,
    received: Counter,
    /// The same, to and from peers on the local network as bandwidth limits see them, and the
    /// rest. Each only grows, as metrics counters must.
    sent_local: AtomicU64,
    sent_remote: AtomicU64,
    received_local: AtomicU64,
    received_remote: AtomicU64,
    chunks_served: AtomicU64,
    requests: Requests,
}

/// Requests this node made of peers, by kind (ADR 0032). Each opens a stream of its own.
#[derive(Debug, Default)]
struct Requests {
    manifests: AtomicU64,
    certificates: AtomicU64,
    availability: AtomicU64,
    replica: AtomicU64,
    chunks: AtomicU64,
    chunks_requested: AtomicU64,
}

/// A request made of a peer, as counted (ADR 0032).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    Manifest,
    /// An author's certificate, to vouch for it (ADR 0024).
    Certificate,
    Availability,
    Replica,
    /// A request for this many chunks.
    Chunks(usize),
}

/// Counts since the node started, for metrics (ADR 0030).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrafficCounts {
    /// Bytes of file content sent to peers on the local network.
    pub sent_local: u64,
    /// Bytes of file content sent to other peers.
    pub sent_remote: u64,
    /// Bytes of file content received from peers on the local network.
    pub received_local: u64,
    /// Bytes of file content received from other peers.
    pub received_remote: u64,
    /// Chunks sent to peers.
    pub chunks_served: u64,
    /// Manifests requested from peers.
    pub manifest_requests: u64,
    /// Certificates requested from peers.
    pub certificate_requests: u64,
    /// Availability queries sent to peers.
    pub availability_requests: u64,
    /// Full-replica queries sent to peers.
    pub replica_requests: u64,
    /// Chunk requests sent to peers, each of up to 64 chunks.
    pub chunk_requests: u64,
    /// Chunks asked for in those requests.
    pub chunks_requested: u64,
}

/// Totals since start and current rates of file content moved, in bytes and bytes per second.
/// Index exchanges and other protocol messages are not counted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transfer {
    /// Bytes of file content sent since the node started.
    pub sent: u64,
    /// Bytes of file content received since the node started.
    pub received: u64,
    /// Bytes per second sent, averaged over the last ten seconds.
    pub send_rate: u64,
    /// Bytes per second received, averaged over the last ten seconds.
    pub receive_rate: u64,
}

impl Traffic {
    pub(crate) fn new() -> Self {
        Self {
            start: Instant::now(),
            sent: Counter::new(),
            received: Counter::new(),
            sent_local: AtomicU64::new(0),
            sent_remote: AtomicU64::new(0),
            received_local: AtomicU64::new(0),
            received_remote: AtomicU64::new(0),
            chunks_served: AtomicU64::new(0),
            requests: Requests::default(),
        }
    }

    fn second(&self) -> u64 {
        self.start.elapsed().as_secs()
    }

    pub(crate) fn sent(&self, bytes: usize, local: bool) {
        self.sent.add(self.second(), bytes as u64);
        let split = if local {
            &self.sent_local
        } else {
            &self.sent_remote
        };
        split.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn received(&self, bytes: usize, local: bool) {
        self.received.add(self.second(), bytes as u64);
        let split = if local {
            &self.received_local
        } else {
            &self.received_remote
        };
        split.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn chunk_served(&self) {
        self.chunks_served.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn request(&self, request: Request) {
        let r = &self.requests;
        let counter = match request {
            Request::Manifest => &r.manifests,
            Request::Certificate => &r.certificates,
            Request::Availability => &r.availability,
            Request::Replica => &r.replica,
            Request::Chunks(n) => {
                r.chunks_requested.fetch_add(n as u64, Ordering::Relaxed);
                &r.chunks
            }
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn counts(&self) -> TrafficCounts {
        TrafficCounts {
            sent_local: self.sent_local.load(Ordering::Relaxed),
            sent_remote: self.sent_remote.load(Ordering::Relaxed),
            received_local: self.received_local.load(Ordering::Relaxed),
            received_remote: self.received_remote.load(Ordering::Relaxed),
            chunks_served: self.chunks_served.load(Ordering::Relaxed),
            manifest_requests: self.requests.manifests.load(Ordering::Relaxed),
            certificate_requests: self.requests.certificates.load(Ordering::Relaxed),
            availability_requests: self.requests.availability.load(Ordering::Relaxed),
            replica_requests: self.requests.replica.load(Ordering::Relaxed),
            chunk_requests: self.requests.chunks.load(Ordering::Relaxed),
            chunks_requested: self.requests.chunks_requested.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn transfer(&self) -> Transfer {
        let second = self.second();
        Transfer {
            sent: self.sent.total.load(Ordering::Relaxed),
            received: self.received.total.load(Ordering::Relaxed),
            send_rate: self.sent.rate(second),
            receive_rate: self.received.rate(second),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_and_rates() {
        let counter = Counter::new();
        for second in 0..20 {
            counter.add(second, 1000);
        }
        counter.add(19, 500);
        assert_eq!(counter.total.load(Ordering::Relaxed), 20_500);
        // Seconds 10 to 19 are full; 20 is under way.
        assert_eq!(counter.rate(20), 1050);
        // Nothing moved for a while.
        assert_eq!(counter.rate(40), 0);
        assert!(counter.recent.lock().unwrap().len() <= WINDOW as usize + 1);
    }

    #[test]
    fn local_and_remote_counts() {
        let traffic = Traffic::new();
        traffic.sent(100, true);
        traffic.sent(40, false);
        traffic.received(7, false);
        traffic.chunk_served();
        traffic.request(Request::Manifest);
        traffic.request(Request::Chunks(8));
        traffic.request(Request::Chunks(3));
        assert_eq!(
            traffic.counts(),
            TrafficCounts {
                sent_local: 100,
                sent_remote: 40,
                received_local: 0,
                received_remote: 7,
                chunks_served: 1,
                manifest_requests: 1,
                chunk_requests: 2,
                chunks_requested: 11,
                ..TrafficCounts::default()
            }
        );
        assert_eq!(traffic.transfer().sent, 140);
    }
}
