//! One replica's metadata for one folder.

use std::collections::HashMap;

use ed25519_dalek::SigningKey;
use tidefell_proto::sync::{is_valid_path, EntryKind, EntryVersion};
use tidefell_proto::{DeviceId, EntryId, FolderId, Signed, VersionVector};

use crate::recycle::{RecycleBin, Recycled, Retention};
use crate::resolve::winner;
use crate::{Error, Result};

/// A version held by this replica, with its signed form and local index sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    /// The decoded version.
    pub version: EntryVersion,
    /// The version exactly as signed by its author, for forwarding to other peers.
    pub signed: Signed<EntryVersion>,
    /// Local index sequence assigned when this replica stored the version.
    pub sequence: u64,
}

/// The versions of one entry that no other version supersedes. They are mutually concurrent;
/// `current` is the winner and the rest are conflict copies.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) current: StoredVersion,
    pub(crate) conflicts: Vec<StoredVersion>,
}

impl Entry {
    fn all(&self) -> impl Iterator<Item = &StoredVersion> {
        std::iter::once(&self.current).chain(&self.conflicts)
    }
}

/// What applying a version did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// The entry was not known before.
    New,
    /// The version superseded every version of the entry.
    Updated,
    /// The version is concurrent with another; the entry now has conflict copies.
    Conflict,
    /// The version was already known or is superseded; nothing changed.
    Stale,
}

/// Metadata for one folder on one replica.
#[derive(Debug, Clone)]
pub struct Folder {
    pub(crate) id: FolderId,
    pub(crate) device: DeviceId,
    pub(crate) epoch: u64,
    pub(crate) sequence: u64,
    pub(crate) entries: HashMap<EntryId, Entry>,
    pub(crate) recycle: RecycleBin,
}

impl Folder {
    /// Creates empty metadata for `id` on `device`.
    ///
    /// `epoch` identifies this index; pick a random non-zero value each time an index is
    /// created from scratch.
    ///
    /// # Panics
    ///
    /// If `epoch` is 0.
    pub fn new(id: FolderId, device: DeviceId, epoch: u64, retention: Retention) -> Self {
        assert_ne!(epoch, 0, "epoch 0 means 'none' on the wire");
        Self {
            id,
            device,
            epoch,
            sequence: 0,
            entries: HashMap::new(),
            recycle: RecycleBin::new(retention),
        }
    }

    /// The folder.
    pub fn id(&self) -> FolderId {
        self.id
    }

    /// This replica's device.
    pub fn device(&self) -> DeviceId {
        self.device
    }

    /// This index's epoch.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Highest index sequence assigned so far.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The current (winning) version of `entry`.
    pub fn current(&self, entry: &EntryId) -> Option<&EntryVersion> {
        self.entries.get(entry).map(|e| &e.current.version)
    }

    /// Conflict copies of `entry`, if any.
    pub fn conflicts(&self, entry: &EntryId) -> &[StoredVersion] {
        self.entries
            .get(entry)
            .map(|e| e.conflicts.as_slice())
            .unwrap_or_default()
    }

    /// Current versions of all entries, including tombstones, in no particular order.
    pub fn entries(&self) -> impl Iterator<Item = &EntryVersion> {
        self.entries.values().map(|e| &e.current.version)
    }

    /// The live (non-deleted) entry currently at `path`, if any.
    pub fn find_path(&self, path: &str) -> Option<&EntryVersion> {
        self.entries()
            .find(|v| v.path == path && !matches!(v.kind, EntryKind::Tombstone))
    }

    /// The recycle bin.
    pub fn recycle_bin(&self) -> &RecycleBin {
        &self.recycle
    }

    /// Changes the recycle bin retention period.
    pub fn set_retention(&mut self, retention: Retention) {
        self.recycle.set_retention(retention);
    }

    /// Removes recycle-bin items whose retention has expired and returns them, so the caller
    /// can release their content.
    pub fn purge_expired(&mut self, now_ms: i64) -> Vec<Recycled> {
        self.recycle.purge_expired(now_ms)
    }

    /// Records a change made on this device and returns the signed version to send to peers.
    ///
    /// The new version supersedes every version of the entry this replica knows, including
    /// conflict copies, which resolves any conflict.
    pub fn local_change(
        &mut self,
        key: &SigningKey,
        entry: EntryId,
        path: &str,
        kind: EntryKind,
        modified_ms: i64,
    ) -> Result<Signed<EntryVersion>> {
        if key.verifying_key().to_bytes() != *self.device.as_bytes() {
            return Err(Error::WrongDevice);
        }
        if !is_valid_path(path) {
            return Err(Error::InvalidPath(path.to_owned()));
        }
        let mut version = VersionVector::new();
        if let Some(existing) = self.entries.get(&entry) {
            for stored in existing.all() {
                version.merge(&stored.version.version);
            }
        }
        version.increment(self.device);
        let new = EntryVersion {
            folder: self.id,
            entry,
            path: path.to_owned(),
            kind,
            modified_ms,
            author: self.device,
            version,
        };
        let signed = Signed::sign(&new, key);
        self.store(new, signed.clone());
        Ok(signed)
    }

    /// Verifies and applies a version received from a peer.
    pub fn apply_remote(&mut self, signed: Signed<EntryVersion>) -> Result<Applied> {
        let version = signed.verify_authored()?;
        if version.folder != self.id {
            return Err(Error::WrongFolder);
        }
        if !is_valid_path(&version.path) {
            return Err(Error::InvalidPath(version.path));
        }
        Ok(self.store(version, signed))
    }

    /// Restores a deleted entry from the recycle bin as a new local change.
    pub fn restore(
        &mut self,
        key: &SigningKey,
        entry: EntryId,
        now_ms: i64,
    ) -> Result<Signed<EntryVersion>> {
        let deleted = matches!(
            self.current(&entry).map(|v| &v.kind),
            Some(EntryKind::Tombstone)
        );
        let item = match self.recycle.get(&entry) {
            Some(item) if deleted => item.clone(),
            _ => return Err(Error::NotRestorable),
        };
        let old = item.version;
        self.local_change(key, entry, &old.path, old.kind, now_ms)
    }

    /// Versions stored after `since`, in sequence order, for an index response. If `epoch`
    /// is not this index's epoch, returns everything. Also returns the `up_to` value to send.
    pub fn index_since(&self, epoch: u64, since: u64) -> (Vec<&StoredVersion>, u64) {
        let since = if epoch == self.epoch { since } else { 0 };
        let mut out: Vec<&StoredVersion> = self
            .entries
            .values()
            .flat_map(Entry::all)
            .filter(|s| s.sequence > since)
            .collect();
        out.sort_by_key(|s| s.sequence);
        (out, self.sequence)
    }

    /// Adds a verified version, keeping only versions no other version supersedes.
    fn store(&mut self, version: EntryVersion, signed: Signed<EntryVersion>) -> Applied {
        let entry_id = version.entry;
        let existing = self.entries.remove(&entry_id);
        let is_new = existing.is_none();

        let mut survivors = Vec::new();
        let mut superseded = Vec::new();
        if let Some(existing) = existing {
            let previous_current = existing.current.version.clone();
            let known = existing
                .all()
                .any(|s| s.version.version.dominates(&version.version));
            if known {
                self.entries.insert(entry_id, existing);
                return Applied::Stale;
            }
            for stored in std::iter::once(existing.current).chain(existing.conflicts) {
                if version.version.dominates(&stored.version.version) {
                    superseded.push(stored.version);
                } else {
                    survivors.push(stored);
                }
            }
            // Recycle the live version a delete removed, preferring the one users saw.
            if matches!(version.kind, EntryKind::Tombstone) {
                let removed = superseded
                    .iter()
                    .find(|v| **v == previous_current)
                    .or_else(|| superseded.first())
                    .filter(|v| !matches!(v.kind, EntryKind::Tombstone));
                if let Some(removed) = removed {
                    self.recycle.insert(Recycled {
                        version: removed.clone(),
                        deleted_at_ms: version.modified_ms,
                    });
                }
            }
        }

        self.sequence += 1;
        survivors.push(StoredVersion {
            version,
            signed,
            sequence: self.sequence,
        });
        let win = winner(survivors.iter().map(|s| &s.version));
        let current = survivors.swap_remove(win);
        survivors.sort_by_key(|s| s.sequence);

        if !matches!(current.version.kind, EntryKind::Tombstone) {
            self.recycle.remove(&entry_id);
        }
        let conflicted = !survivors.is_empty();
        self.entries.insert(
            entry_id,
            Entry {
                current,
                conflicts: survivors,
            },
        );
        match (is_new, conflicted) {
            (true, _) => Applied::New,
            (false, true) => Applied::Conflict,
            (false, false) => Applied::Updated,
        }
    }
}
