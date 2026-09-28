//! File content sent and received, for status displays (ADR 0030).

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
        let mut recent = self.recent.lock().unwrap();
        match recent.back_mut() {
            Some((s, b)) if *s == second => *b += bytes,
            _ => recent.push_back((second, bytes)),
        }
        while recent.front().is_some_and(|(s, _)| *s + WINDOW < second) {
            recent.pop_front();
        }
    }

    /// Bytes per second over the last full seconds, not counting the one under way.
    fn rate(&self, second: u64) -> u64 {
        let recent = self.recent.lock().unwrap();
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
        }
    }

    fn second(&self) -> u64 {
        self.start.elapsed().as_secs()
    }

    pub(crate) fn sent(&self, bytes: usize) {
        self.sent.add(self.second(), bytes as u64);
    }

    pub(crate) fn received(&self, bytes: usize) {
        self.received.add(self.second(), bytes as u64);
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
}
