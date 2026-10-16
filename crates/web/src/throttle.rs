//! Slowing down guessing: a sign-in or a link password that keeps failing waits longer each
//! time, doubling from a second to fifteen minutes, per account or link rather than per
//! address, which a reverse proxy would hide. The wait caps what a lock-out by a stranger who
//! knows a username can cost.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Failures that cost nothing but the fixed delay: a typo or two.
const FREE_FAILURES: u32 = 3;
/// The longest wait.
const MAX_WAIT: Duration = Duration::from_secs(15 * 60);
/// Entries not touched for this long are forgotten.
const FORGET_AFTER: Duration = Duration::from_secs(60 * 60);
/// Most keys tracked; past it, keys not waiting are forgotten, and if all are waiting a new key
/// waits too, rather than the map growing with every name tried.
const MAX_KEYS: usize = 10_000;
/// What a new key waits when every tracked key is waiting.
const FULL_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug)]
struct Entry {
    failures: u32,
    until: Instant,
    touched: Instant,
}

#[derive(Debug, Default)]
pub(crate) struct Throttle {
    entries: Mutex<HashMap<String, Entry>>,
}

impl Throttle {
    /// Starts an attempt for `key`, counted as a failure until [`Self::cleared`] says it was
    /// not, so attempts made at the same time cannot all go before the first one fails. Returns
    /// how long `key` waits if this one fails, or, if it must wait now, how much longer.
    pub(crate) fn begin(&self, key: &str) -> Result<Duration, Duration> {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap();
        let stale = |e: &Entry| now.duration_since(e.touched) >= FORGET_AFTER;
        if entries.get(key).is_some_and(stale) {
            entries.remove(key);
        }
        if let Some(entry) = entries.get(key) {
            if entry.until > now {
                return Err(entry.until - now);
            }
        } else if entries.len() >= MAX_KEYS {
            entries.retain(|_, e| e.until > now && !stale(e));
            if entries.len() >= MAX_KEYS {
                return Err(FULL_WAIT);
            }
        }
        let entry = entries.entry(key.to_owned()).or_insert(Entry {
            failures: 0,
            until: now,
            touched: now,
        });
        entry.failures = entry.failures.saturating_add(1);
        entry.touched = now;
        let wait = match entry.failures.checked_sub(FREE_FAILURES + 1) {
            None => Duration::ZERO,
            Some(over) => Duration::from_secs(1u64 << over.min(16)).min(MAX_WAIT),
        };
        entry.until = now + wait;
        Ok(wait)
    }

    /// `key` got it right: nothing is held against it.
    pub(crate) fn cleared(&self, key: &str) {
        self.entries.lock().unwrap().remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_double_after_the_free_failures_and_cap() {
        let throttle = Throttle::default();
        for _ in 0..FREE_FAILURES {
            assert_eq!(throttle.begin("login:a"), Ok(Duration::ZERO));
        }
        assert_eq!(throttle.begin("login:a"), Ok(Duration::from_secs(1)));
        assert!(throttle.begin("login:a").is_err(), "waiting");
        assert!(throttle.begin("login:b").is_ok(), "per key");
        throttle.cleared("login:a");
        assert_eq!(throttle.begin("login:a"), Ok(Duration::ZERO));
    }

    #[test]
    fn the_wait_doubles_up_to_the_cap() {
        let throttle = Throttle::default();
        let mut waits = Vec::new();
        for _ in 0..30 {
            let wait = throttle.begin("k").unwrap();
            // As if the wait had passed.
            throttle.entries.lock().unwrap().get_mut("k").unwrap().until = Instant::now();
            waits.push(wait.as_secs());
        }
        assert_eq!(&waits[..7], &[0, 0, 0, 1, 2, 4, 8]);
        assert_eq!(*waits.last().unwrap(), MAX_WAIT.as_secs());
    }

    #[test]
    fn many_keys_do_not_grow_it_without_bound() {
        let throttle = Throttle::default();
        for i in 0..MAX_KEYS {
            let key = i.to_string();
            for _ in 0..=FREE_FAILURES {
                throttle.begin(&key).unwrap();
            }
        }
        let later = Instant::now() + Duration::from_secs(60);
        for entry in throttle.entries.lock().unwrap().values_mut() {
            entry.until = later;
        }
        assert_eq!(
            throttle.begin("new"),
            Err(FULL_WAIT),
            "every key is waiting"
        );
        throttle
            .entries
            .lock()
            .unwrap()
            .values_mut()
            .next()
            .unwrap()
            .until = Instant::now();
        assert!(throttle.begin("new").is_ok(), "a key not waiting made room");
        assert!(throttle.entries.lock().unwrap().len() <= MAX_KEYS);
    }
}
