//! One replica's metadata for one folder.

use std::collections::HashMap;
use std::sync::OnceLock;

use ed25519_dalek::SigningKey;
use tidefell_proto::sync::{is_valid_path, EntryKind, EntryVersion};
use tidefell_proto::{Causality, DeviceId, EntryId, FolderId, Signed, VersionVector};

use crate::layout::Layout;
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
    /// Where live entries go on disk, computed on demand and cleared when entries change.
    pub(crate) layout: OnceLock<Layout>,
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
            layout: OnceLock::new(),
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

    /// The live (non-deleted) entry on disk at `path`, if any. Entries that share a path are
    /// told apart by the [`Layout`], so this is the entry whose file is at `path`, which for a
    /// displaced entry is not its recorded path.
    pub fn find_path(&self, path: &str) -> Option<&EntryVersion> {
        self.layout()
            .entry_at(path)
            .and_then(|entry| self.current(&entry))
    }

    /// Where every live entry goes on disk (ADR 0024).
    pub fn layout(&self) -> &Layout {
        self.layout
            .get_or_init(|| Layout::new(self.entries.values().map(|e| &e.current.version)))
    }

    /// Where `entry` goes on disk, if it is live.
    pub fn disk_path(&self, entry: &EntryId) -> Option<&str> {
        self.layout().path_of(entry)
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

    /// Removes `entry` from this replica's recycle bin, so it can no longer be restored here,
    /// and returns it so the caller can release its content. Other replicas keep their copy.
    pub fn discard(&mut self, entry: &EntryId) -> Option<Recycled> {
        self.recycle.remove(entry)
    }

    /// Empties this replica's recycle bin and returns what it held. Other replicas keep
    /// their copies.
    pub fn discard_all(&mut self) -> Vec<Recycled> {
        self.recycle.take_all()
    }

    /// Records a local change built on everything this replica knows of the entry: its
    /// current version and conflict copies. Use [`Folder::local_change_from`] when the change
    /// was made to content on disk whose version is known.
    pub fn local_change(
        &mut self,
        key: &SigningKey,
        entry: EntryId,
        path: &str,
        kind: EntryKind,
        modified_ms: i64,
    ) -> Result<Signed<EntryVersion>> {
        let base = self.known_vector(&entry);
        self.local_change_from(key, entry, &base, path, kind, modified_ms)
            .map(|(signed, _)| signed)
    }

    /// The merge of the vectors of the entry's current version and conflict copies: the base
    /// of content that reflects all of them. Empty for an unknown entry.
    pub fn known_vector(&self, entry: &EntryId) -> VersionVector {
        let mut vector = VersionVector::new();
        if let Some(existing) = self.entries.get(entry) {
            for stored in existing.all() {
                vector.merge(&stored.version.version);
            }
        }
        vector
    }

    /// Records a local change made to content whose version vector was `base` (ADR 0024).
    ///
    /// The new version supersedes only what `base` covers, so a remote version that arrived
    /// after that content was written stays concurrent and becomes a conflict copy instead of
    /// being lost. This device's own counter still moves past every version it has made.
    /// Returns the signed version to send to peers, and its vector: the base of the content
    /// the change describes.
    pub fn local_change_from(
        &mut self,
        key: &SigningKey,
        entry: EntryId,
        base: &VersionVector,
        path: &str,
        kind: EntryKind,
        modified_ms: i64,
    ) -> Result<(Signed<EntryVersion>, VersionVector)> {
        if key.verifying_key().to_bytes() != *self.device.as_bytes() {
            return Err(Error::WrongDevice);
        }
        if !is_valid_path(path) {
            return Err(Error::InvalidPath(path.to_owned()));
        }
        let mut version = base.clone();
        let own = self.known_vector(&entry).get(&self.device);
        if own > version.get(&self.device) {
            version.merge(&[(self.device, own)].into_iter().collect());
        }
        version
            .increment(self.device)
            .ok_or(Error::CounterCeiling)?;
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
        let vector = new.version.clone();
        self.store(new, signed.clone());
        Ok((signed, vector))
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
        if !version.version.within_ceiling() {
            return Err(Error::CounterCeiling);
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

    /// At most `limit` versions stored after `since`, in sequence order, for an index
    /// response. If `epoch` is not this index's epoch, starts from the beginning. Also returns
    /// the `up_to` value to send: the sequence of the last version returned if more follow,
    /// otherwise the index's current sequence.
    pub fn index_since(&self, epoch: u64, since: u64, limit: usize) -> (Vec<&StoredVersion>, u64) {
        let since = if epoch == self.epoch { since } else { 0 };
        let mut out: Vec<&StoredVersion> = self
            .entries
            .values()
            .flat_map(Entry::all)
            .filter(|s| s.sequence > since)
            .collect();
        if out.len() > limit {
            out.select_nth_unstable_by_key(limit, |s| s.sequence);
            out.truncate(limit);
            out.sort_by_key(|s| s.sequence);
            let up_to = out.last().map_or(since, |s| s.sequence);
            return (out, up_to);
        }
        out.sort_by_key(|s| s.sequence);
        (out, self.sequence)
    }

    /// Adds a verified version, keeping only versions no other version supersedes.
    fn store(&mut self, version: EntryVersion, signed: Signed<EntryVersion>) -> Applied {
        let entry_id = version.entry;
        self.layout.take();
        let existing = self.entries.remove(&entry_id);
        let is_new = existing.is_none();

        let mut survivors = Vec::new();
        let mut superseded = Vec::new();
        if let Some(existing) = existing {
            let previous_current = existing.current.version.clone();
            // Known if a stored version is later, or is this very version. Equal vectors with
            // different content are concurrent (ADR 0024).
            let known = existing.all().any(|s| {
                s.version == version
                    || s.version.version.compare(&version.version) == Causality::After
            });
            if known {
                self.entries.insert(entry_id, existing);
                return Applied::Stale;
            }
            for stored in std::iter::once(existing.current).chain(existing.conflicts) {
                if version.version.compare(&stored.version.version) == Causality::After {
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
