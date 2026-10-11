//! Maps and sets that remember which keys changed, so a save writes only those (ADR 0034).
//!
//! Reading goes through the inner collection. Every way of changing one is a method here, so
//! a change that would not be saved does not compile.

use std::collections::{btree_map, BTreeMap, BTreeSet};

use serde::de::DeserializeOwned;
use serde::Serialize;

/// A key's change, encoded: its new value, or `None` if it was removed.
pub(crate) type Changed = (Vec<u8>, Option<Vec<u8>>);

#[expect(
    clippy::expect_used,
    reason = "postcard fails to encode only sequences of unknown length, which none of these are"
)]
pub(crate) fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_stdvec(value).expect("in-memory encoding cannot fail")
}

/// A map that remembers which keys changed.
#[derive(Debug, Clone)]
pub(crate) struct Tracked<K: Ord, V> {
    map: BTreeMap<K, V>,
    changed: BTreeSet<K>,
}

impl<K: Ord, V> Default for Tracked<K, V> {
    fn default() -> Self {
        Self {
            map: BTreeMap::new(),
            changed: BTreeSet::new(),
        }
    }
}

impl<K: Ord + Clone, V> Tracked<K, V> {
    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.changed.insert(key.clone());
        self.map.insert(key, value)
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Ord + ?Sized,
    {
        let (key, value) = self.map.remove_entry(key)?;
        self.changed.insert(key);
        Some(value)
    }

    pub(crate) fn entry(&mut self, key: K) -> btree_map::Entry<'_, K, V> {
        self.changed.insert(key.clone());
        self.map.entry(key)
    }

    /// Removes and returns everything.
    pub(crate) fn take_all(&mut self) -> BTreeMap<K, V> {
        self.changed.extend(self.map.keys().cloned());
        std::mem::take(&mut self.map)
    }

    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&K, &mut V) -> bool) {
        let changed = &mut self.changed;
        self.map.retain(|key, value| {
            let kept = keep(key, value);
            if !kept {
                changed.insert(key.clone());
            }
            kept
        });
    }
}

impl<K: Ord + Serialize + DeserializeOwned, V: Serialize + DeserializeOwned> Tracked<K, V> {
    /// The keys that changed since last asked, encoded with their values now.
    pub(crate) fn take_changed(&mut self) -> Vec<Changed> {
        std::mem::take(&mut self.changed)
            .into_iter()
            .map(|key| {
                let value = self.map.get(&key).map(encode);
                (encode(&key), value)
            })
            .collect()
    }

    /// Everything, encoded, as if every key had changed.
    pub(crate) fn all(&self) -> Vec<Changed> {
        self.map
            .iter()
            .map(|(k, v)| (encode(k), Some(encode(v))))
            .collect()
    }

    /// Loads what [`Tracked::all`] and [`Tracked::take_changed`] wrote.
    pub(crate) fn decode(records: Vec<(Vec<u8>, Vec<u8>)>) -> Result<Self, postcard::Error> {
        let map = records
            .into_iter()
            .map(|(k, v)| Ok((postcard::from_bytes(&k)?, postcard::from_bytes(&v)?)))
            .collect::<Result<_, postcard::Error>>()?;
        Ok(Self {
            map,
            changed: BTreeSet::new(),
        })
    }
}

impl<K: Ord, V> FromIterator<(K, V)> for Tracked<K, V> {
    /// A map of what was saved before: nothing in it has changed.
    fn from_iter<I: IntoIterator<Item = (K, V)>>(items: I) -> Self {
        Self {
            map: items.into_iter().collect(),
            changed: BTreeSet::new(),
        }
    }
}

impl<K: Ord, V> std::ops::Deref for Tracked<K, V> {
    type Target = BTreeMap<K, V>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

/// A set that remembers which members were added or removed.
#[derive(Debug, Clone)]
pub(crate) struct TrackedSet<K: Ord> {
    set: BTreeSet<K>,
    changed: BTreeSet<K>,
}

impl<K: Ord> Default for TrackedSet<K> {
    fn default() -> Self {
        Self {
            set: BTreeSet::new(),
            changed: BTreeSet::new(),
        }
    }
}

impl<K: Ord + Clone> TrackedSet<K> {
    pub(crate) fn insert(&mut self, key: K) -> bool {
        let added = self.set.insert(key.clone());
        if added {
            self.changed.insert(key);
        }
        added
    }

    pub(crate) fn remove<Q>(&mut self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: Ord + ?Sized,
    {
        match self.set.take(key) {
            Some(key) => {
                self.changed.insert(key);
                true
            }
            None => false,
        }
    }
}

impl<K: Ord + Serialize + DeserializeOwned> TrackedSet<K> {
    /// The members added or removed since last asked, encoded; a member is `Some` if present.
    pub(crate) fn take_changed(&mut self) -> Vec<Changed> {
        std::mem::take(&mut self.changed)
            .into_iter()
            .map(|key| {
                let present = self.set.contains(&key).then(Vec::new);
                (encode(&key), present)
            })
            .collect()
    }

    /// Every member, encoded.
    pub(crate) fn all(&self) -> Vec<Changed> {
        self.set
            .iter()
            .map(|k| (encode(k), Some(Vec::new())))
            .collect()
    }

    /// Loads what [`TrackedSet::all`] and [`TrackedSet::take_changed`] wrote.
    pub(crate) fn decode(records: Vec<(Vec<u8>, Vec<u8>)>) -> Result<Self, postcard::Error> {
        let set = records
            .into_iter()
            .map(|(k, _)| postcard::from_bytes(&k))
            .collect::<Result<_, postcard::Error>>()?;
        Ok(Self {
            set,
            changed: BTreeSet::new(),
        })
    }
}

impl<K: Ord> FromIterator<K> for TrackedSet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(items: I) -> Self {
        Self {
            set: items.into_iter().collect(),
            changed: BTreeSet::new(),
        }
    }
}

impl<K: Ord> std::ops::Deref for TrackedSet<K> {
    type Target = BTreeSet<K>;

    fn deref(&self) -> &Self::Target {
        &self.set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_changed_is_handed_over() {
        let mut map: Tracked<String, u32> = [("a".to_owned(), 1), ("b".to_owned(), 2)]
            .into_iter()
            .collect();
        assert!(map.take_changed().is_empty());
        map.insert("c".to_owned(), 3);
        *map.entry("a".to_owned()).or_default() += 10;
        map.remove("b");
        *map.entry("d".to_owned()).or_default() += 4;
        map.retain(|k, _| k != "d");
        let changed = map.take_changed();
        assert_eq!(changed.len(), 4);
        let loaded = Tracked::<String, u32>::decode(
            map.all()
                .into_iter()
                .map(|(k, v)| (k, v.unwrap()))
                .collect(),
        )
        .unwrap();
        assert_eq!(*loaded, *map);
        assert!(map.take_changed().is_empty());

        let mut set: TrackedSet<u8> = [1, 2].into_iter().collect();
        assert!(!set.insert(1));
        set.insert(3);
        set.remove(&2);
        set.remove(&1);
        let changed = set.take_changed();
        assert_eq!(changed.len(), 3);
        assert_eq!(changed.iter().filter(|(_, v)| v.is_some()).count(), 1);
    }
}
