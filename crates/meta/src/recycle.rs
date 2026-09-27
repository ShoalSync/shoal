//! The recycle bin: deleted entries kept until their retention period expires.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use tidefell_proto::sync::EntryVersion;
use tidefell_proto::EntryId;

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// How long deleted entries are kept. Chosen at first setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Retention {
    /// Keep deleted entries for this many days.
    Days(u32),
    /// Never purge automatically.
    Forever,
}

impl Retention {
    /// A retention of `days` days, where 0 means forever: the convention of the REST API, the
    /// configuration file and the bindings.
    pub fn from_days(days: u32) -> Self {
        if days == 0 {
            Self::Forever
        } else {
            Self::Days(days)
        }
    }

    /// The number of days, or `None` for forever.
    pub fn days(self) -> Option<u32> {
        match self {
            Self::Days(days) => Some(days),
            Self::Forever => None,
        }
    }
}

/// A deleted entry: its last live version and when it was deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recycled {
    /// The last live version before the delete.
    pub version: EntryVersion,
    /// When this device recorded the delete, in milliseconds since the Unix epoch by its own
    /// clock. Retention counts from here.
    pub deleted_at_ms: i64,
}

/// Deleted entries of one folder, by entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycleBin {
    retention: Retention,
    items: BTreeMap<EntryId, Recycled>,
}

impl RecycleBin {
    /// An empty bin with the given retention.
    pub fn new(retention: Retention) -> Self {
        Self {
            retention,
            items: BTreeMap::new(),
        }
    }

    /// The retention period.
    pub fn retention(&self) -> Retention {
        self.retention
    }

    /// Changes the retention period. Takes effect at the next [`RecycleBin::purge_expired`].
    pub fn set_retention(&mut self, retention: Retention) {
        self.retention = retention;
    }

    /// When `item` expires, or `None` if it never does.
    pub fn expires_at_ms(&self, item: &Recycled) -> Option<i64> {
        match self.retention {
            Retention::Days(days) => Some(
                item.deleted_at_ms
                    .saturating_add(i64::from(days).saturating_mul(DAY_MS)),
            ),
            Retention::Forever => None,
        }
    }

    pub(crate) fn insert(&mut self, item: Recycled) {
        self.items.insert(item.version.entry, item);
    }

    pub(crate) fn remove(&mut self, entry: &EntryId) -> Option<Recycled> {
        self.items.remove(entry)
    }

    /// The recycled item for `entry`, if any.
    pub fn get(&self, entry: &EntryId) -> Option<&Recycled> {
        self.items.get(entry)
    }

    /// All recycled items, by entry.
    pub fn iter(&self) -> impl Iterator<Item = &Recycled> {
        self.items.values()
    }

    /// Number of recycled items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True if the bin is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Removes and returns every item. The caller releases their content.
    pub(crate) fn take_all(&mut self) -> Vec<Recycled> {
        std::mem::take(&mut self.items).into_values().collect()
    }

    /// When the next item expires, or `None` if none does.
    pub fn next_expiry_ms(&self) -> Option<i64> {
        self.items
            .values()
            .filter_map(|item| self.expires_at_ms(item))
            .min()
    }

    /// Removes and returns items whose retention expired at or before `now_ms`. The caller
    /// releases their content.
    pub fn purge_expired(&mut self, now_ms: i64) -> Vec<Recycled> {
        let expired: Vec<EntryId> = self
            .items
            .iter()
            .filter(|(_, item)| self.expires_at_ms(item).is_some_and(|t| t <= now_ms))
            .map(|(entry, _)| *entry)
            .collect();
        expired
            .iter()
            .filter_map(|entry| self.items.remove(entry))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Retention;

    #[test]
    fn zero_days_is_forever() {
        assert_eq!(Retention::from_days(0), Retention::Forever);
        assert_eq!(Retention::from_days(30), Retention::Days(30));
        assert_eq!(Retention::Days(30).days(), Some(30));
        assert_eq!(Retention::Forever.days(), None);
    }
}
