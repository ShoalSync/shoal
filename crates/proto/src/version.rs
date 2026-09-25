//! Version vectors: per-device change counters used to order versions of an entry.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::DeviceId;

/// The largest counter a version vector may hold (ADR 0024). Versions with a larger counter are
/// invalid, and a device refuses to make a change that would pass it.
pub const MAX_COUNTER: u64 = 1 << 62;

/// How two version vectors relate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Causality {
    /// Both vectors are identical.
    Equal,
    /// The left vector happened before the right one.
    Before,
    /// The left vector happened after the right one.
    After,
    /// Neither dominates: the versions are in conflict.
    Concurrent,
}

/// A map from device to the number of changes that device has made. Absent devices count as 0.
///
/// Encoded as a map sorted by `DeviceId`, with no zero counters, so every vector has exactly one
/// encoding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionVector(BTreeMap<DeviceId, u64>);

impl VersionVector {
    /// An empty vector.
    pub fn new() -> Self {
        Self::default()
    }

    /// The counter for `device`.
    pub fn get(&self, device: &DeviceId) -> u64 {
        self.0.get(device).copied().unwrap_or(0)
    }

    /// Increments `device`'s counter and returns the new value, or `None`, leaving the vector
    /// unchanged, if that would pass [`MAX_COUNTER`].
    pub fn increment(&mut self, device: DeviceId) -> Option<u64> {
        let next = self
            .get(&device)
            .checked_add(1)
            .filter(|&n| n <= MAX_COUNTER)?;
        self.0.insert(device, next);
        Some(next)
    }

    /// True if no counter exceeds [`MAX_COUNTER`].
    pub fn within_ceiling(&self) -> bool {
        self.0.values().all(|&c| c <= MAX_COUNTER)
    }

    /// Sets every counter to the maximum of `self` and `other`.
    pub fn merge(&mut self, other: &VersionVector) {
        for (device, &counter) in &other.0 {
            let mine = self.0.entry(*device).or_insert(0);
            *mine = (*mine).max(counter);
        }
    }

    /// Compares two vectors.
    pub fn compare(&self, other: &VersionVector) -> Causality {
        let mut less = false;
        let mut greater = false;
        for device in self.0.keys().chain(other.0.keys()) {
            match self.get(device).cmp(&other.get(device)) {
                Ordering::Less => less = true,
                Ordering::Greater => greater = true,
                Ordering::Equal => {}
            }
        }
        match (less, greater) {
            (false, false) => Causality::Equal,
            (true, false) => Causality::Before,
            (false, true) => Causality::After,
            (true, true) => Causality::Concurrent,
        }
    }

    /// True if every counter in `self` is at least the one in `other`.
    pub fn dominates(&self, other: &VersionVector) -> bool {
        matches!(self.compare(other), Causality::Equal | Causality::After)
    }

    /// Iterates over non-zero counters in device order.
    pub fn iter(&self) -> impl Iterator<Item = (&DeviceId, &u64)> {
        self.0.iter()
    }
}

impl FromIterator<(DeviceId, u64)> for VersionVector {
    fn from_iter<I: IntoIterator<Item = (DeviceId, u64)>>(iter: I) -> Self {
        Self(iter.into_iter().filter(|&(_, c)| c > 0).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(n: u8) -> DeviceId {
        DeviceId([n; 32])
    }

    fn vv(pairs: &[(u8, u64)]) -> VersionVector {
        pairs.iter().map(|&(n, c)| (d(n), c)).collect()
    }

    #[test]
    fn ordering() {
        assert_eq!(vv(&[]).compare(&vv(&[])), Causality::Equal);
        assert_eq!(vv(&[(1, 1)]).compare(&vv(&[(1, 2)])), Causality::Before);
        assert_eq!(vv(&[(1, 2)]).compare(&vv(&[(1, 1)])), Causality::After);
        assert_eq!(vv(&[(1, 1)]).compare(&vv(&[(2, 1)])), Causality::Concurrent);
        assert_eq!(vv(&[(1, 0)]).compare(&vv(&[])), Causality::Equal);
    }

    #[test]
    fn increment_stops_at_the_ceiling() {
        let mut a = vv(&[(1, MAX_COUNTER - 1)]);
        assert!(a.within_ceiling());
        assert_eq!(a.increment(d(1)), Some(MAX_COUNTER));
        assert_eq!(a.increment(d(1)), None);
        assert_eq!(a.get(&d(1)), MAX_COUNTER, "unchanged when refused");
        assert!(!vv(&[(2, MAX_COUNTER + 1)]).within_ceiling());
        assert!(!vv(&[(2, u64::MAX)]).within_ceiling());
    }

    #[test]
    fn merge_takes_maximum() {
        let mut a = vv(&[(1, 3), (2, 1)]);
        a.merge(&vv(&[(2, 5), (3, 1)]));
        assert_eq!(a, vv(&[(1, 3), (2, 5), (3, 1)]));
    }

    #[test]
    fn increment() {
        let mut a = VersionVector::new();
        assert_eq!(a.increment(d(1)), Some(1));
        assert_eq!(a.increment(d(1)), Some(2));
        assert!(a.dominates(&vv(&[(1, 1)])));
    }
}
