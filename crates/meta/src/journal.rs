//! What a folder's metadata changed since it was last saved, so a database stores only that
//! (ADR 0034), and rebuilding a folder from what was stored.
//!
//! Stored versions are kept by the sequence this replica stored them under: storing appends,
//! and a version superseded later is recorded as dropped rather than removed in place. Which
//! version of an entry is current is not stored either: it is the winner of the entry's kept
//! versions, as [`Folder`] chooses it when storing.

use std::collections::{BTreeSet, HashMap};

use tidefell_proto::sync::EntryVersion;
use tidefell_proto::{DeviceId, EntryId, FolderId, PublicKey, Signed, SignedKind};

use crate::folder::{Entry, StoredVersion};
use crate::recycle::{RecycleBin, Recycled, Retention};
use crate::resolve::winner;
use crate::{Error, Folder, Result};

/// A folder's identity and counters: what it is besides its versions and recycle bin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The folder.
    pub id: FolderId,
    /// This replica's device.
    pub device: DeviceId,
    /// The index epoch.
    pub epoch: u64,
    /// The last sequence stored under.
    pub sequence: u64,
    /// How long deleted files are kept.
    pub retention: Retention,
}

/// What changed since the journal was last taken.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Journal {
    /// Versions stored, with their sequences, in sequence order.
    pub stored: Vec<(u64, Signed<EntryVersion>)>,
    /// Sequences of versions stored before and no longer kept.
    pub dropped: Vec<u64>,
    /// Recycle-bin items by entry: set, or removed (`None`).
    pub recycle: Vec<(EntryId, Option<Recycled>)>,
}

impl Journal {
    /// True if nothing changed.
    pub fn is_empty(&self) -> bool {
        self.stored.is_empty() && self.dropped.is_empty() && self.recycle.is_empty()
    }
}

/// What a folder changed since its journal was last taken. Not persisted.
#[derive(Debug, Clone, Default)]
pub(crate) struct Pending {
    /// Versions stored, by entry and sequence.
    stored: Vec<(EntryId, u64)>,
    /// Sequences of versions no longer kept.
    dropped: Vec<u64>,
    /// Recycle-bin entries set or removed.
    recycle: BTreeSet<EntryId>,
}

impl Pending {
    pub(crate) fn stored(&mut self, entry: EntryId, sequence: u64) {
        self.stored.push((entry, sequence));
    }

    pub(crate) fn dropped(&mut self, sequence: u64) {
        self.dropped.push(sequence);
    }

    pub(crate) fn recycled(&mut self, entry: EntryId) {
        self.recycle.insert(entry);
    }
}

impl Folder {
    /// The folder's identity and counters.
    pub fn header(&self) -> Header {
        Header {
            id: self.id,
            device: self.device,
            epoch: self.epoch,
            sequence: self.sequence,
            retention: self.recycle.retention(),
        }
    }

    /// What changed since the journal was last taken, which this clears. A version stored and
    /// superseded in between is in neither list.
    pub fn take_journal(&mut self) -> Journal {
        let pending = std::mem::take(&mut self.pending);
        let new: BTreeSet<u64> = pending.stored.iter().map(|(_, s)| *s).collect();
        let dropped_new: BTreeSet<u64> = pending
            .dropped
            .iter()
            .copied()
            .filter(|s| new.contains(s))
            .collect();
        // Each new version's signed form moves to the journal: once saved, the database keeps
        // it rather than memory (ADR 0035).
        let mut stored: Vec<(u64, Signed<EntryVersion>)> = pending
            .stored
            .iter()
            .filter(|(_, s)| !dropped_new.contains(s))
            .filter_map(|(entry, sequence)| {
                let entry = self.entries.get_mut(entry)?;
                let kept = std::iter::once(&mut entry.current)
                    .chain(entry.conflicts.iter_mut())
                    .find(|v| v.sequence == *sequence)?;
                Some((*sequence, *kept.signed.take()?))
            })
            .collect();
        stored.sort_by_key(|(s, _)| *s);
        let mut dropped: Vec<u64> = pending
            .dropped
            .into_iter()
            .filter(|s| !new.contains(s))
            .collect();
        dropped.sort_unstable();
        let recycle = pending
            .recycle
            .into_iter()
            .map(|entry| (entry, self.recycle.get(&entry).cloned()))
            .collect();
        Journal {
            stored,
            dropped,
            recycle,
        }
    }

    /// Everything the folder holds, as a journal from nothing, to store it whole. Clears the
    /// journal.
    /// Only versions not saved before are in it: a folder made from a database's records has
    /// its versions there already.
    pub fn full_journal(&mut self) -> Journal {
        self.pending = Pending::default();
        let mut stored: Vec<(u64, Signed<EntryVersion>)> = self
            .entries
            .values_mut()
            .flat_map(|e| std::iter::once(&mut e.current).chain(e.conflicts.iter_mut()))
            .filter_map(|v| Some((v.sequence, *v.signed.take()?)))
            .collect();
        stored.sort_by_key(|(s, _)| *s);
        Journal {
            stored,
            dropped: Vec::new(),
            recycle: self
                .recycle
                .iter()
                .map(|item| (item.version.entry, Some(item.clone())))
                .collect(),
        }
    }

    /// Every version the folder keeps, by sequence and [`crate::copy_id`], to compare with
    /// what was saved.
    pub fn stored_ids(&self) -> Vec<(u64, EntryId)> {
        let mut ids: Vec<(u64, EntryId)> = self
            .entries
            .values()
            .flat_map(Entry::all)
            .map(|v| (v.sequence, v.id))
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Rebuilds a folder from what a journal stored: its header, the versions it still keeps,
    /// and its recycle bin. Signatures are not checked again, as they were when each version
    /// was received; each version must be for this folder and signed by its author's key.
    pub fn from_records(
        header: Header,
        versions: impl IntoIterator<Item = (u64, Signed<EntryVersion>)>,
        recycled: impl IntoIterator<Item = Recycled>,
    ) -> Result<Self> {
        let mut entries: HashMap<EntryId, Entry> = HashMap::new();
        for (sequence, signed) in versions {
            let version = EntryVersion::decode(&signed.body).map_err(Error::Snapshot)?;
            if version.folder != header.id {
                return Err(Error::WrongFolder);
            }
            if PublicKey::from(version.author) != signed.signer {
                return Err(tidefell_proto::Error::SignerMismatch.into());
            }
            if sequence > header.sequence {
                return Err(Error::UnsupportedFormat);
            }
            // The database keeps the signed form; memory keeps the version (ADR 0035).
            let stored = StoredVersion {
                id: crate::copy_id(&signed),
                version,
                signed: None,
                sequence,
            };
            match entries.entry(stored.version.entry) {
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(Entry {
                        current: stored,
                        conflicts: Vec::new(),
                    });
                }
                std::collections::hash_map::Entry::Occupied(mut slot) => {
                    slot.get_mut().conflicts.push(stored);
                }
            }
        }
        // An entry with several versions: the winner is current, the rest in sequence order.
        for entry in entries.values_mut().filter(|e| !e.conflicts.is_empty()) {
            let mut versions: Vec<StoredVersion> = std::mem::take(&mut entry.conflicts);
            versions.push(entry.current.clone());
            let win = winner(versions.iter().map(|v| &v.version));
            entry.current = versions.swap_remove(win);
            versions.sort_by_key(|v| v.sequence);
            entry.conflicts = versions;
        }
        entries.shrink_to_fit();
        let mut recycle = RecycleBin::new(header.retention);
        for item in recycled {
            recycle.insert(item);
        }
        let mut folder = Folder::new(
            header.id,
            header.device,
            header.epoch.max(1),
            header.retention,
        );
        folder.epoch = header.epoch;
        folder.sequence = header.sequence;
        folder.entries = entries;
        folder.recycle = recycle;
        Ok(folder)
    }
}
