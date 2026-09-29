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

/// The entry ID under which the version `signed` is kept once it loses a conflict: in the
/// recycle bin, and as its own file when both are kept or it is restored (ADR 0029). Derived
/// from the version, so every replica, and every device that keeps both, uses the same ID.
pub fn copy_id(signed: &Signed<EntryVersion>) -> EntryId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"tidefell conflict copy\n");
    hasher.update(&signed.body);
    let mut id = [0; 16];
    id.copy_from_slice(&hasher.finalize().as_bytes()[..16]);
    EntryId(id)
}

/// True if a conflict copy of kind `copy` needs no decision beside a current version of kind
/// `current`: it has the same content, or it is a delete that a live version beat.
fn settled(copy: &EntryKind, current: Option<&EntryKind>) -> bool {
    Some(copy) == current
        || (matches!(copy, EntryKind::Tombstone)
            && current.is_some_and(|c| !matches!(c, EntryKind::Tombstone)))
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
    /// What stored versions did to their entries since [`Folder::take_changes`] was last
    /// called, for the activity log (ADR 0030). Not persisted.
    pub(crate) changes: std::cell::RefCell<Vec<Change>>,
}

/// What a stored version did to its entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A new file.
    Added,
    /// New content.
    Changed,
    /// Deleted.
    Deleted,
    /// Moved to another path, perhaps with new content.
    Renamed,
    /// Live again after it was deleted.
    Restored,
    /// A version made apart from the current one arrived, and waits for a decision.
    Conflict,
}

/// One change to a folder, for its activity log (ADR 0030).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// The entry.
    pub entry: EntryId,
    /// What happened.
    pub kind: ChangeKind,
    /// Its path now.
    pub path: String,
    /// Its path before, for a rename.
    pub old_path: Option<String>,
    /// The device that made the change.
    pub author: DeviceId,
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
            changes: std::cell::RefCell::default(),
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

    /// Conflict copies of `entry` that differ from its current version: those the user has to
    /// decide about. A copy with the same content is not a conflict, and neither is a delete
    /// that an edit beat, which stays automatic (ADR 0029).
    pub fn open_conflicts(&self, entry: &EntryId) -> impl Iterator<Item = &StoredVersion> {
        let current = self.current(entry).map(|v| &v.kind);
        self.conflicts(entry)
            .iter()
            .filter(move |c| !settled(&c.version.kind, current))
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

    /// Records a local change built on the entry's current content: see
    /// [`Folder::current_vector`]. Conflict copies with other content stay (ADR 0029). Use
    /// [`Folder::local_change_from`] when the change was made to content on disk whose version
    /// is known.
    pub fn local_change(
        &mut self,
        key: &SigningKey,
        entry: EntryId,
        path: &str,
        kind: EntryKind,
        modified_ms: i64,
    ) -> Result<Signed<EntryVersion>> {
        let base = self.current_vector(&entry);
        self.local_change_from(key, entry, &base, path, kind, modified_ms)
            .map(|(signed, _)| signed)
    }

    /// The vector of the entry's current content: the current version's, merged with the
    /// conflict copies that are not open conflicts (see [`Folder::open_conflicts`]). Content
    /// written from the current version has this base, so a change to it supersedes all of
    /// those and no open conflict. Empty for an unknown entry.
    pub fn current_vector(&self, entry: &EntryId) -> VersionVector {
        let mut vector = VersionVector::new();
        if let Some(existing) = self.entries.get(entry) {
            let current = Some(&existing.current.version.kind);
            for stored in existing.all() {
                if settled(&stored.version.kind, current) {
                    vector.merge(&stored.version.version);
                }
            }
        }
        vector
    }

    /// The merge of the vectors of the entry's current version and conflict copies: the base
    /// of a change that supersedes all of them, such as resolving a conflict. Empty for an
    /// unknown entry.
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
    /// being lost. The change is numbered after every change this device has made to the entry,
    /// and a change of this device's that `base` has not seen stays unseen: the vector has a
    /// hole there (ADR 0029).
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
        let own = self
            .known_vector(&entry)
            .get(&self.device)
            .max(base.get(&self.device));
        let next = own
            .checked_add(1)
            .filter(|&n| n <= tidefell_proto::MAX_COUNTER)
            .ok_or(Error::CounterCeiling)?;
        let mut version = base.clone();
        version.add(self.device, next);
        if !version.within_ceiling() {
            return Err(Error::TooManyConflicts);
        }
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
        self.store(new, signed.clone(), modified_ms);
        Ok((signed, vector))
    }

    /// Verifies and applies a version received from a peer at `received_ms`, this device's time.
    /// An entry the version deletes enters the recycle bin at `received_ms`, not at the
    /// tombstone's modification time, which comes from the deleting device's clock: retention
    /// belongs to each device (ADR 0021), and a device with a wrong clock must not make every
    /// other device purge its deletes early or late.
    pub fn apply_remote(
        &mut self,
        signed: Signed<EntryVersion>,
        received_ms: i64,
    ) -> Result<Applied> {
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
        Ok(self.store(version, signed, received_ms))
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

    /// The changes stored versions made since the last call, oldest first (ADR 0030).
    pub fn take_changes(&self) -> Vec<Change> {
        std::mem::take(&mut *self.changes.borrow_mut())
    }

    /// Notes what the version just stored did: it is the current version now, or it waits as a
    /// conflict. `before` is the entry's current version before it.
    fn note_change(
        &self,
        before: Option<&EntryVersion>,
        current: &StoredVersion,
        others: &[StoredVersion],
    ) {
        let newest = others
            .iter()
            .chain(std::iter::once(current))
            .max_by_key(|s| s.sequence)
            .expect("the version just stored");
        let after = &current.version;
        let live = |v: &EntryVersion| !matches!(v.kind, EntryKind::Tombstone);
        let kind = if newest.sequence != current.sequence {
            // It lost to the current version: a conflict, unless it needs no decision.
            (!settled(&newest.version.kind, Some(&after.kind))).then_some(ChangeKind::Conflict)
        } else if before.is_some_and(|b| {
            others.iter().any(|s| s.version == *b) && !settled(&b.kind, Some(&after.kind))
        }) {
            // It won over the version it was made apart from, which now waits for a decision.
            Some(ChangeKind::Conflict)
        } else {
            match before {
                None => live(after).then_some(ChangeKind::Added),
                Some(b) if !live(b) && live(after) => Some(ChangeKind::Restored),
                Some(b) if live(b) && !live(after) => Some(ChangeKind::Deleted),
                Some(b) if live(b) && b.path != after.path => Some(ChangeKind::Renamed),
                Some(b) if live(b) && b.kind != after.kind => Some(ChangeKind::Changed),
                _ => None,
            }
        };
        if let Some(kind) = kind {
            self.changes.borrow_mut().push(Change {
                entry: after.entry,
                kind,
                path: after.path.clone(),
                old_path: before
                    .filter(|_| kind == ChangeKind::Renamed)
                    .map(|b| b.path.clone()),
                author: newest.version.author,
            });
        }
    }

    /// Restores a version kept in the recycle bin after it lost a conflict, as a new file at
    /// `path`, and returns the signed version to send to peers. `path` is normally
    /// `<stem> (<device>, <date>)<extension>` beside the file it was a version of.
    pub fn restore_copy(
        &mut self,
        key: &SigningKey,
        id: EntryId,
        path: &str,
        now_ms: i64,
    ) -> Result<Signed<EntryVersion>> {
        let item = match self.recycle.get(&id) {
            Some(item) if item.conflict_of.is_some() && !self.entries.contains_key(&id) => {
                item.clone()
            }
            _ => return Err(Error::NotRestorable),
        };
        let (signed, _) = self.local_change_from(
            key,
            id,
            &VersionVector::new(),
            path,
            item.version.kind,
            now_ms,
        )?;
        Ok(signed)
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

    /// Adds a verified version, recorded on this device at `recorded_ms`, keeping only versions
    /// no other version supersedes.
    fn store(
        &mut self,
        version: EntryVersion,
        signed: Signed<EntryVersion>,
        recorded_ms: i64,
    ) -> Applied {
        let entry_id = version.entry;
        self.layout.take();
        let existing = self.entries.remove(&entry_id);
        let is_new = existing.is_none();
        let before = existing.as_ref().map(|e| e.current.version.clone());

        let mut survivors = Vec::new();
        let mut superseded = Vec::new();
        let mut lost = Vec::new();
        if let Some(existing) = existing {
            let previous_current = existing.current.version.clone();
            // The versions still undecided: the current version and its open conflicts.
            let open: Vec<Vec<u8>> = existing
                .all()
                .filter(|s| {
                    s.version == previous_current
                        || !settled(&s.version.kind, Some(&previous_current.kind))
                })
                .map(|s| s.signed.body.clone())
                .collect();
            // What the new version was made from: its vector without its own change.
            let mut made_from = version.version.clone();
            made_from.remove(version.author, version.version.get(&version.author));
            // A version's line: it and the versions with its content, or deletes. A change made
            // from within that line is an edit of it; one that has seen more decided between it
            // and something else.
            let line = |kind: &EntryKind| {
                let mut vector = VersionVector::new();
                for s in existing.all() {
                    if s.version.kind == *kind || matches!(s.version.kind, EntryKind::Tombstone) {
                        vector.merge(&s.version.version);
                    }
                }
                vector
            };
            let live = !matches!(version.kind, EntryKind::Tombstone);
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
            // An undecided version with other content that a live version replaces after
            // seeing more than its line lost a conflict (ADR 0029). It goes to the recycle bin.
            let lines: Vec<(Vec<u8>, VersionVector)> = existing
                .all()
                .map(|s| (s.signed.body.clone(), line(&s.version.kind)))
                .collect();
            for stored in std::iter::once(existing.current).chain(existing.conflicts) {
                if version.version.compare(&stored.version.version) == Causality::After {
                    let decided = live
                        && open.contains(&stored.signed.body)
                        && stored.version.kind != version.kind
                        && !matches!(stored.version.kind, EntryKind::Tombstone)
                        && lines
                            .iter()
                            .find(|(body, _)| *body == stored.signed.body)
                            .is_some_and(|(_, line)| !line.dominates(&made_from));
                    if decided {
                        lost.push(stored.clone());
                    }
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
                        deleted_at_ms: recorded_ms,
                        conflict_of: None,
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
        for stored in lost {
            let id = copy_id(&stored.signed);
            // Kept both: the copy is, or was, an entry of its own.
            if self.entries.contains_key(&id) || id == entry_id {
                continue;
            }
            let mut version = stored.version;
            version.entry = id;
            self.recycle.insert(Recycled {
                version,
                deleted_at_ms: recorded_ms,
                conflict_of: Some(entry_id),
            });
        }
        self.note_change(before.as_ref(), &current, &survivors);
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
