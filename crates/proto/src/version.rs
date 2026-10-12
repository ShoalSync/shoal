//! Version vectors: the set of changes to an entry that a version has seen, used to order
//! versions of that entry.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::DeviceId;

/// The largest counter a version vector may hold (ADR 0024). Versions with a larger counter are
/// invalid, and a device refuses to make a change that would pass it.
pub const MAX_COUNTER: u64 = 1 << 62;

/// The most holes a version vector may have (ADR 0029). Each is a change of the entry that a
/// version has not seen while it has seen later changes by the same device.
pub const MAX_HOLES: usize = 64;

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

/// The changes to an entry that a version has seen. Each device numbers its changes 1, 2, 3
/// and so on, and a vector holds, per device, the highest change seen and the ranges of lower
/// changes not seen (holes). Absent devices count as none seen.
///
/// Encoded as a map sorted by `DeviceId` with no zero counters, then the holes, sorted by
/// device and first change, as inclusive ranges that are disjoint, not adjacent, and below that
/// device's highest change. So every vector has exactly one encoding.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionVector {
    counters: Counters,
    holes: Vec<(DeviceId, u64, u64)>,
}

/// The highest change seen per device, sorted by device: a map encoded exactly as a `BTreeMap`
/// is, kept as a vector because most have one or two devices, and every version and every file a
/// device keeps holds one (ADR 0035).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct Counters(Vec<(DeviceId, u64)>);

impl Counters {
    fn find(&self, device: &DeviceId) -> Result<usize, usize> {
        self.0.binary_search_by(|(d, _)| d.cmp(device))
    }

    fn get(&self, device: &DeviceId) -> Option<&u64> {
        self.find(device)
            .ok()
            .and_then(|i| self.0.get(i))
            .map(|(_, c)| c)
    }

    fn insert(&mut self, device: DeviceId, counter: u64) {
        match self.find(&device) {
            Ok(i) => {
                if let Some(slot) = self.0.get_mut(i) {
                    slot.1 = counter;
                }
            }
            Err(i) => {
                self.0.reserve_exact(1);
                self.0.insert(i, (device, counter));
            }
        }
    }

    fn remove(&mut self, device: &DeviceId) {
        if let Ok(i) = self.find(device) {
            self.0.remove(i);
        }
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn keys(&self) -> impl Iterator<Item = &DeviceId> {
        self.0.iter().map(|(d, _)| d)
    }

    fn values(&self) -> impl Iterator<Item = &u64> {
        self.0.iter().map(|(_, c)| c)
    }

    fn iter(&self) -> impl Iterator<Item = (&DeviceId, &u64)> {
        self.0.iter().map(|(d, c)| (d, c))
    }
}

impl FromIterator<(DeviceId, u64)> for Counters {
    /// Sorted by device; a device given twice keeps the last counter, as a map would.
    fn from_iter<I: IntoIterator<Item = (DeviceId, u64)>>(iter: I) -> Self {
        let map: BTreeMap<DeviceId, u64> = iter.into_iter().collect();
        Self(map.into_iter().collect())
    }
}

impl Serialize for Counters {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de> Deserialize<'de> for Counters {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = BTreeMap::<DeviceId, u64>::deserialize(deserializer)?;
        Ok(map.into_iter().collect())
    }
}

/// Inclusive ranges of changes, ascending, disjoint and not adjacent.
type Ranges = Vec<(u64, u64)>;

/// Adds `(first, last)` to `ranges`.
fn insert(ranges: &mut Ranges, first: u64, last: u64) {
    ranges.push((first, last));
    ranges.sort_unstable();
    let mut out: Ranges = Vec::with_capacity(ranges.len());
    for &(a, b) in ranges.iter() {
        match out.last_mut() {
            Some(prev) if a <= prev.1.saturating_add(1) => prev.1 = prev.1.max(b),
            _ => out.push((a, b)),
        }
    }
    *ranges = out;
}

/// True if every change in `small` is in `big`.
fn covers(big: &Ranges, small: &Ranges) -> bool {
    small
        .iter()
        .all(|&(a, b)| big.iter().any(|&(x, y)| x <= a && b <= y))
}

impl VersionVector {
    /// An empty vector.
    pub fn new() -> Self {
        Self::default()
    }

    /// The vector of every change by `device` up to `counter`, and none by any other device.
    pub fn single(device: DeviceId, counter: u64) -> Self {
        let mut vector = Self::new();
        vector.counters.insert(device, counter);
        vector
    }

    /// The device and counter of a vector [`VersionVector::single`] makes, if this is one: most
    /// versions' vectors are, and can be kept in less memory (ADR 0035).
    pub fn as_single(&self) -> Option<(DeviceId, u64)> {
        match self.counters.0.as_slice() {
            [(device, counter)] if self.holes.is_empty() => Some((*device, *counter)),
            _ => None,
        }
    }

    /// The highest change by `device` seen, or 0.
    pub fn get(&self, device: &DeviceId) -> u64 {
        self.counters.get(device).copied().unwrap_or(0)
    }

    /// True if nothing has been seen.
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }

    /// True if the vector has holes.
    pub fn has_holes(&self) -> bool {
        !self.holes.is_empty()
    }

    /// True if change `n` by `device` has been seen.
    pub fn contains(&self, device: &DeviceId, n: u64) -> bool {
        n >= 1
            && n <= self.get(device)
            && !self
                .holes
                .iter()
                .any(|(d, a, b)| d == device && *a <= n && n <= *b)
    }

    /// The changes by `device` seen, as ranges.
    fn ranges(&self, device: &DeviceId) -> Ranges {
        let max = self.get(device);
        let mut out = Vec::new();
        let mut next = 1;
        for &(_, a, b) in self.holes.iter().filter(|(d, ..)| d == device) {
            // A peer's vector may not be checked yet: saturate rather than overflow.
            if a > next {
                out.push((next, a.saturating_sub(1)));
            }
            next = b.saturating_add(1);
        }
        if max >= next {
            out.push((next, max));
        }
        out
    }

    /// Replaces the changes seen from `device` with `ranges`.
    fn set_ranges(&mut self, device: DeviceId, ranges: &Ranges) {
        self.holes.retain(|(d, ..)| *d != device);
        match ranges.last() {
            None => {
                self.counters.remove(&device);
            }
            Some(&(_, max)) => {
                self.counters.insert(device, max);
                let mut next = 1;
                for &(a, b) in ranges {
                    if a > next {
                        self.holes.push((device, next, a.saturating_sub(1)));
                    }
                    next = b.saturating_add(1);
                }
                self.holes.sort_unstable();
            }
        }
    }

    /// Adds change `n` by `device`.
    pub fn add(&mut self, device: DeviceId, n: u64) {
        if n == 0 || self.contains(&device, n) {
            return;
        }
        let mut ranges = self.ranges(&device);
        insert(&mut ranges, n, n);
        self.set_ranges(device, &ranges);
    }

    /// Removes change `n` by `device`, if seen.
    pub fn remove(&mut self, device: DeviceId, n: u64) {
        if !self.contains(&device, n) {
            return;
        }
        let ranges: Ranges = self
            .ranges(&device)
            .into_iter()
            .flat_map(|(a, b)| {
                if a <= n && n <= b {
                    vec![(a, n.saturating_sub(1)), (n.saturating_add(1), b)]
                } else {
                    vec![(a, b)]
                }
            })
            .filter(|(a, b)| a <= b)
            .collect();
        self.set_ranges(device, &ranges);
    }

    /// Adds `device`'s next change after the highest seen and returns its number, or `None`,
    /// leaving the vector unchanged, if that would pass [`MAX_COUNTER`].
    pub fn increment(&mut self, device: DeviceId) -> Option<u64> {
        let next = self
            .get(&device)
            .checked_add(1)
            .filter(|&n| n <= MAX_COUNTER)?;
        self.counters.insert(device, next);
        Some(next)
    }

    /// True if no counter exceeds [`MAX_COUNTER`], there are at most [`MAX_HOLES`] holes, and
    /// the holes are in their one encoding.
    pub fn within_ceiling(&self) -> bool {
        let counters = self.counters.values().all(|&c| c > 0 && c <= MAX_COUNTER);
        let holes = self.holes.len() <= MAX_HOLES
            && self
                .holes
                .iter()
                .all(|(d, a, b)| *a >= 1 && a <= b && *b < self.get(d))
            && self.holes.windows(2).all(|w| match w {
                [(d1, _, b1), (d2, a2, _)] => d1 < d2 || (d1 == d2 && b1.saturating_add(1) < *a2),
                _ => true,
            });
        counters && holes
    }

    /// Adds every change `other` has seen.
    pub fn merge(&mut self, other: &VersionVector) {
        if !self.has_holes() && !other.has_holes() {
            for (device, &counter) in other.counters.iter() {
                if counter > self.get(device) {
                    self.counters.insert(*device, counter);
                }
            }
            return;
        }
        for device in other.counters.keys() {
            let mut ranges = self.ranges(device);
            for (a, b) in other.ranges(device) {
                insert(&mut ranges, a, b);
            }
            self.set_ranges(*device, &ranges);
        }
    }

    /// Compares two vectors as sets of changes.
    pub fn compare(&self, other: &VersionVector) -> Causality {
        let mut less = false;
        let mut greater = false;
        let plain = !self.has_holes() && !other.has_holes();
        for device in self.counters.keys().chain(other.counters.keys()) {
            if plain {
                match self.get(device).cmp(&other.get(device)) {
                    std::cmp::Ordering::Less => less = true,
                    std::cmp::Ordering::Greater => greater = true,
                    std::cmp::Ordering::Equal => {}
                }
                continue;
            }
            let (mine, theirs) = (self.ranges(device), other.ranges(device));
            if mine == theirs {
                continue;
            }
            match (covers(&mine, &theirs), covers(&theirs, &mine)) {
                (true, _) => greater = true,
                (_, true) => less = true,
                _ => {
                    less = true;
                    greater = true;
                }
            }
        }
        match (less, greater) {
            (false, false) => Causality::Equal,
            (true, false) => Causality::Before,
            (false, true) => Causality::After,
            (true, true) => Causality::Concurrent,
        }
    }

    /// True if `self` has seen every change `other` has.
    pub fn dominates(&self, other: &VersionVector) -> bool {
        matches!(self.compare(other), Causality::Equal | Causality::After)
    }

    /// The highest change per device, in device order, holes aside.
    pub fn iter(&self) -> impl Iterator<Item = (&DeviceId, &u64)> {
        self.counters.iter()
    }
}

impl FromIterator<(DeviceId, u64)> for VersionVector {
    /// A vector that has seen changes 1 to n of each device.
    fn from_iter<I: IntoIterator<Item = (DeviceId, u64)>>(iter: I) -> Self {
        Self {
            counters: iter.into_iter().filter(|&(_, c)| c > 0).collect(),
            holes: Vec::new(),
        }
    }
}

/// A version vector as encoded before vectors had holes: engine state format 8 and meta
/// snapshot format 1.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyVersionVector(pub BTreeMap<DeviceId, u64>);

impl From<LegacyVersionVector> for VersionVector {
    fn from(legacy: LegacyVersionVector) -> Self {
        legacy.0.into_iter().collect()
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
    fn unchecked_holes_do_not_overflow() {
        // A peer's vector, not yet checked against the ceiling, with a hole to the very end.
        let mut v = vv(&[(1, 5)]);
        v.holes.push((d(1), 2, u64::MAX));
        assert!(!v.within_ceiling());
        let mut other = v.clone();
        other.add(d(1), 7);
        other.remove(d(1), 1);
        let mut merged = vv(&[(1, 3)]);
        merged.merge(&v);
        let _ = v.compare(&merged);
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

    #[test]
    fn holes() {
        let a = d(1);
        // Seen A's changes 1 and 3, not 2.
        let mut v = vv(&[(1, 1), (2, 1)]);
        v.add(a, 3);
        assert!(v.contains(&a, 1) && !v.contains(&a, 2) && v.contains(&a, 3));
        assert_eq!(v.get(&a), 3);
        assert!(v.has_holes() && v.within_ceiling());
        // Concurrent with the version that made change 2.
        let two = vv(&[(1, 2)]);
        assert_eq!(v.compare(&two), Causality::Concurrent);
        assert_eq!(two.compare(&v), Causality::Concurrent);
        // A later change on top still has not seen it.
        let mut later = v.clone();
        later.add(a, 4);
        assert_eq!(later.compare(&two), Causality::Concurrent);
        assert_eq!(later.compare(&v), Causality::After);
        // Merging fills the hole, and the encoding is the plain one again.
        let mut merged = later.clone();
        merged.merge(&two);
        assert!(!merged.has_holes());
        assert_eq!(merged, vv(&[(1, 4), (2, 1)]));
        assert_eq!(
            postcard::to_stdvec(&merged).unwrap(),
            postcard::to_stdvec(&vv(&[(1, 4), (2, 1)])).unwrap()
        );
        assert_eq!(merged.compare(&two), Causality::After);
    }

    #[test]
    fn removing_changes() {
        let mut v = vv(&[(1, 5), (2, 1)]);
        v.remove(d(1), 5);
        assert_eq!(v, vv(&[(1, 4), (2, 1)]));
        v.remove(d(1), 2);
        assert!(!v.contains(&d(1), 2) && v.contains(&d(1), 3));
        v.remove(d(2), 1);
        assert_eq!(v.get(&d(2)), 0);
        assert!(v.within_ceiling());
    }

    #[test]
    fn hole_encoding_is_canonical() {
        let mut v = vv(&[(1, 1)]);
        v.add(d(1), 5);
        v.add(d(1), 3);
        assert_eq!(v.holes, vec![(d(1), 2, 2), (d(1), 4, 4)]);
        assert!(v.within_ceiling());
        let bad = |holes: Vec<(DeviceId, u64, u64)>| VersionVector {
            counters: [(d(1), 9)].into_iter().collect::<Counters>(),
            holes,
        };
        assert!(!bad(vec![(d(1), 0, 1)]).within_ceiling(), "change 0");
        assert!(!bad(vec![(d(1), 3, 2)]).within_ceiling(), "backwards");
        assert!(
            !bad(vec![(d(1), 5, 9)]).within_ceiling(),
            "the highest change"
        );
        assert!(
            !bad(vec![(d(1), 2, 3), (d(1), 4, 5)]).within_ceiling(),
            "adjacent"
        );
        assert!(
            !bad(vec![(d(1), 4, 5), (d(1), 2, 2)]).within_ceiling(),
            "unsorted"
        );
        assert!(!bad(vec![(d(2), 1, 1)]).within_ceiling(), "unknown device");
        let many = (0..=MAX_HOLES as u64).map(|i| (d(1), 2 * i + 1, 2 * i + 1));
        let many = VersionVector {
            counters: [(d(1), 1000)].into_iter().collect::<Counters>(),
            holes: many.collect(),
        };
        assert!(!many.within_ceiling(), "too many holes");
    }

    #[test]
    fn counters_encode_as_the_map_they_replaced() {
        let counters = [(d(3), 7), (d(1), 2), (d(2), 5), (d(1), 4)];
        let map: BTreeMap<DeviceId, u64> = counters.into_iter().collect();
        let vector: Counters = counters.into_iter().collect();
        let encoded = postcard::to_stdvec(&vector).unwrap();
        assert_eq!(encoded, postcard::to_stdvec(&map).unwrap());
        assert_eq!(postcard::from_bytes::<Counters>(&encoded).unwrap(), vector);
        assert_eq!(vector.get(&d(1)), Some(&4));
    }
}
