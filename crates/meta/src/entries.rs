//! A folder's entries, kept densely (ADR 0035): side by side in one vector, found by ID through
//! a table of their positions. A hash map of entries leaves a third or more of its slots empty,
//! each the size of an entry. An entry is never removed, as a delete keeps its tombstone, so
//! positions never change.

use std::hash::BuildHasher;

use hashbrown::HashTable;
use shoal_proto::EntryId;
use std::collections::hash_map::RandomState;

use crate::folder::Entry;

#[derive(Debug, Clone, Default)]
pub(crate) struct Entries {
    slots: Vec<Entry>,
    /// Positions in `slots`, by their entry's ID.
    index: HashTable<u32>,
    hasher: RandomState,
}

impl Entries {
    fn find(&self, id: &EntryId) -> Option<usize> {
        let hash = self.hasher.hash_one(id);
        self.index
            .find(hash, |&slot| {
                self.slots
                    .get(slot as usize)
                    .is_some_and(|e| e.current.entry == *id)
            })
            .map(|&slot| slot as usize)
    }

    /// The entry at position `slot`, as [`Entries::values`] lists them.
    pub(crate) fn at(&self, slot: u32) -> Option<&Entry> {
        self.slots.get(slot as usize)
    }

    pub(crate) fn get(&self, id: &EntryId) -> Option<&Entry> {
        self.slots.get(self.find(id)?)
    }

    pub(crate) fn get_mut(&mut self, id: &EntryId) -> Option<&mut Entry> {
        let slot = self.find(id)?;
        self.slots.get_mut(slot)
    }

    pub(crate) fn contains_key(&self, id: &EntryId) -> bool {
        self.find(id).is_some()
    }

    /// Every entry, in no particular order.
    pub(crate) fn values(&self) -> std::slice::Iter<'_, Entry> {
        self.slots.iter()
    }

    pub(crate) fn values_mut(&mut self) -> std::slice::IterMut<'_, Entry> {
        self.slots.iter_mut()
    }

    /// Makes room for `more` entries.
    pub(crate) fn reserve(&mut self, more: usize) {
        self.slots.reserve_exact(more);
        let Self {
            slots,
            index,
            hasher,
        } = self;
        index.reserve(more, |&slot| hash_of(hasher, slots, slot));
    }

    /// Stores `entry`, in place of the one with its ID if there is one.
    pub(crate) fn insert(&mut self, entry: Entry) {
        if let Some(slot) = self.find(&entry.current.entry) {
            if let Some(old) = self.slots.get_mut(slot) {
                *old = entry;
            }
            return;
        }
        let hash = self.hasher.hash_one(entry.current.entry);
        let slot = u32::try_from(self.slots.len()).expect("fewer than 2^32 entries");
        self.slots.push(entry);
        let Self {
            slots,
            index,
            hasher,
        } = self;
        index.insert_unique(hash, slot, |&slot| hash_of(hasher, slots, slot));
    }
}

fn hash_of(hasher: &RandomState, slots: &[Entry], slot: u32) -> u64 {
    slots
        .get(slot as usize)
        .map_or(0, |e| hasher.hash_one(e.current.entry))
}
