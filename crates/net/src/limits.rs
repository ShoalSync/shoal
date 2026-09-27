//! Upload and download rate limits (ADR 0029): one bucket each way for the whole device.

use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Duration;

use iroh::endpoint::Connection;
use iroh::TransportAddr;
use tokio::time::Instant;

/// The lowest limit: a 1 MiB chunk must still arrive well within a request's deadline.
pub const MIN_BYTES_PER_SEC: u64 = 64 * 1024;

/// A request is served only while the upload backlog is below this, so that together with one
/// chunk at the lowest limit it stays within the requester's deadline; beyond it the responder
/// answers `Busy`.
pub(crate) const MAX_BACKLOG: Duration = Duration::from_secs(10);

/// How fast this device may send and receive file content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Limits {
    /// Upload limit in bytes per second; `None` for no limit.
    pub upload: Option<u64>,
    /// Download limit in bytes per second; `None` for no limit.
    pub download: Option<u64>,
    /// Also limit traffic to peers on the local network, which is not limited by default.
    pub limit_local: bool,
}

impl Limits {
    /// The limits with any rate below [`MIN_BYTES_PER_SEC`] raised to it.
    pub fn clamped(self) -> Self {
        let clamp = |rate: Option<u64>| rate.map(|r| r.max(MIN_BYTES_PER_SEC));
        Self {
            upload: clamp(self.upload),
            download: clamp(self.download),
            limit_local: self.limit_local,
        }
    }
}

/// One direction's limit: a virtual clock that each transfer moves forward by its size over the
/// rate.
#[derive(Debug)]
struct Bucket {
    /// When the bytes already reserved will have gone at the limit.
    free_at: Mutex<Instant>,
}

impl Bucket {
    fn new() -> Self {
        Self {
            free_at: Mutex::new(Instant::now()),
        }
    }

    /// Reserves `bytes` at `rate` and returns how long to wait before sending them.
    fn reserve(&self, bytes: usize, rate: u64) -> Duration {
        let now = Instant::now();
        let mut free_at = self.free_at.lock().unwrap();
        let start = (*free_at).max(now);
        *free_at = start + Duration::from_secs_f64(bytes as f64 / rate as f64);
        start - now
    }

    /// Like [`Bucket::reserve`], but only while the backlog is at most `max`, checked and
    /// reserved under one lock so concurrent senders cannot together overshoot it. Otherwise
    /// returns how long to wait before trying again.
    fn try_reserve(&self, bytes: usize, rate: u64, max: Duration) -> Result<Duration, Duration> {
        let now = Instant::now();
        let mut free_at = self.free_at.lock().unwrap();
        let backlog = free_at.saturating_duration_since(now);
        if backlog > max {
            return Err(retry_after(backlog));
        }
        let start = (*free_at).max(now);
        *free_at = start + Duration::from_secs_f64(bytes as f64 / rate as f64);
        Ok(start - now)
    }

    /// How long the bytes already reserved will take to go.
    fn backlog(&self) -> Duration {
        self.free_at
            .lock()
            .unwrap()
            .saturating_duration_since(Instant::now())
    }

    /// Forgets what was reserved, as when the rate changes.
    fn reset(&self) {
        *self.free_at.lock().unwrap() = Instant::now();
    }
}

/// When a busy sender is worth asking again: once its backlog has drained to half the budget,
/// and never sooner than a second, so requesters do not storm it.
fn retry_after(backlog: Duration) -> Duration {
    backlog
        .saturating_sub(MAX_BACKLOG / 2)
        .max(Duration::from_secs(1))
}

/// The device's limits and their buckets, shared by every connection.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    limits: Mutex<Limits>,
    upload: Bucket,
    download: Bucket,
}

impl RateLimiter {
    pub(crate) fn new(limits: Limits) -> Self {
        Self {
            limits: Mutex::new(limits.clamped()),
            upload: Bucket::new(),
            download: Bucket::new(),
        }
    }

    pub(crate) fn limits(&self) -> Limits {
        *self.limits.lock().unwrap()
    }

    pub(crate) fn set(&self, limits: Limits) {
        *self.limits.lock().unwrap() = limits.clamped();
        // A backlog reserved at the old rate would hold up the new one.
        self.upload.reset();
        self.download.reset();
    }

    fn applies(&self, conn: &Connection) -> bool {
        self.limits().limit_local || !is_local(conn)
    }

    /// Before reading a chunk to send on `conn`: `Err(retry_after)` if the upload backlog is
    /// already too long, so a busy answer costs no disk read.
    pub(crate) fn upload_busy(&self, conn: &Connection) -> Result<(), Duration> {
        if self.limits().upload.is_some() && self.applies(conn) {
            let backlog = self.upload.backlog();
            if backlog > MAX_BACKLOG {
                return Err(retry_after(backlog));
            }
        }
        Ok(())
    }

    /// Before sending a chunk of `bytes` on `conn`: `Ok(wait)` to send it after `wait`, or
    /// `Err(retry_after)` if the upload backlog is too long to send it in time.
    pub(crate) fn upload(&self, conn: &Connection, bytes: usize) -> Result<Duration, Duration> {
        match self.limits().upload.filter(|_| self.applies(conn)) {
            Some(rate) => self.upload.try_reserve(bytes, rate, MAX_BACKLOG),
            None => Ok(Duration::ZERO),
        }
    }

    /// After receiving a chunk of `bytes` on `conn`: how long to wait before reading more.
    pub(crate) fn download(&self, conn: &Connection, bytes: usize) -> Duration {
        match self.limits().download.filter(|_| self.applies(conn)) {
            Some(rate) => self.download.reserve(bytes, rate),
            None => Duration::ZERO,
        }
    }
}

/// Whether `conn` currently goes directly to an address on the local network.
fn is_local(conn: &Connection) -> bool {
    conn.paths()
        .iter()
        .find(|p| p.is_selected())
        .is_some_and(|p| matches!(p.remote_addr(), TransportAddr::Ip(a) if is_lan(a.ip())))
}

/// Private, link-local and loopback addresses. Carrier-grade NAT addresses (100.64.0.0/10), as
/// used by overlay networks such as Tailscale, can be anywhere and do not count.
fn is_lan(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_unique_local() || v6.is_unicast_link_local() || v6.is_loopback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn transfers_are_spaced_by_the_rate() {
        let bucket = Bucket::new();
        let rate = 100_000;
        assert_eq!(bucket.reserve(100_000, rate), Duration::ZERO);
        // The next 100 kB waits for the first to have gone at 100 kB/s.
        assert_eq!(bucket.reserve(50_000, rate), Duration::from_secs(1));
        assert_eq!(bucket.backlog(), Duration::from_millis(1500));
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(bucket.backlog(), Duration::ZERO);
        assert_eq!(bucket.reserve(1, rate), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_backlog_turns_senders_away_for_a_while() {
        let bucket = Bucket::new();
        let rate = 100_000;
        // Ten seconds of backlog is still accepted, then no more.
        for _ in 0..11 {
            assert!(bucket.try_reserve(100_000, rate, MAX_BACKLOG).is_ok());
        }
        let retry = bucket.try_reserve(1, rate, MAX_BACKLOG).unwrap_err();
        // Back once the eleven seconds queued have drained to five.
        assert_eq!(retry, Duration::from_secs(6));
        // Never sooner than a second.
        assert_eq!(
            retry_after(MAX_BACKLOG / 2 + Duration::from_millis(1)),
            Duration::from_secs(1)
        );
        bucket.reset();
        assert_eq!(bucket.try_reserve(1, rate, MAX_BACKLOG), Ok(Duration::ZERO));
    }

    #[test]
    fn rates_have_a_floor() {
        let limits = Limits {
            upload: Some(1000),
            download: Some(10 * MIN_BYTES_PER_SEC),
            limit_local: false,
        }
        .clamped();
        assert_eq!(limits.upload, Some(MIN_BYTES_PER_SEC));
        assert_eq!(limits.download, Some(10 * MIN_BYTES_PER_SEC));
    }

    #[test]
    fn local_addresses() {
        for lan in [
            "192.168.1.2",
            "10.0.0.1",
            "172.16.5.4",
            "169.254.1.1",
            "127.0.0.1",
            "fd00::1",
            "fe80::1",
            "::1",
            "::ffff:192.168.0.9",
        ] {
            assert!(is_lan(lan.parse().unwrap()), "{lan}");
        }
        for wan in ["8.8.8.8", "100.64.0.1", "2001:db8::1", "172.32.0.1"] {
            assert!(!is_lan(wan.parse().unwrap()), "{wan}");
        }
    }
}
