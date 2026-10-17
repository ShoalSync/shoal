//! What a folder's metadata changed since it was last saved, so a database stores only that
//! (ADR 0034), and rebuilding a folder from what was stored.
//!
//! Stored versions are kept by the sequence this replica stored them under: storing appends,
//! and a version superseded later is recorded as dropped rather than removed in place. Which
//! version of an entry is current is not stored either: it is the winner of the entry's kept
//! versions, as [`Folder`] chooses it when storing.

use std::collections::BTreeSet;

use tidefell_proto::sync::EntryVersion;
use tidefell_proto::{DeviceId, EntryId, FolderId, PublicKey, Signed, SignedKind};

use crate::compact::Slim;
use crate::folder::{Conflicts, Entry, StoredVersion};
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
    /// When versions in `stored` made elsewhere were received, by sequence, this device's
    /// time (ADR 0040). A version without one was made here, or received while rebuilding.
    pub received: Vec<(u64, i64)>,
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
    /// When versions stored were received, by sequence.
    received: Vec<(u64, i64)>,
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

    pub(crate) fn received(&mut self, sequence: u64, received_ms: i64) {
        self.received.push((sequence, received_ms));
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

    /// When the version stored at `sequence` was received, if it was received since the journal
    /// was last taken: what is not saved yet (ADR 0040).
    pub fn pending_receipt(&self, sequence: u64) -> Option<i64> {
        self.pending
            .received
            .iter()
            .rev()
            .find(|(s, _)| *s == sequence)
            .map(|(_, at)| *at)
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
                let signed = if entry.current.sequence == *sequence {
                    &mut entry.current.signed
                } else {
                    &mut entry
                        .conflicts
                        .iter_mut()
                        .find(|v| v.sequence == *sequence)?
                        .signed
                };
                Some((*sequence, *signed.take()?))
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
        // Only for versions saved now: one stored and superseded in between is not.
        let received = pending
            .received
            .into_iter()
            .filter(|(s, _)| stored.binary_search_by_key(s, |(s, _)| *s).is_ok())
            .collect();
        Journal {
            stored,
            dropped,
            recycle,
            received,
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
            .flat_map(|e| {
                std::iter::once((e.current.sequence, &mut e.current.signed))
                    .chain(e.conflicts.iter_mut().map(|v| (v.sequence, &mut v.signed)))
            })
            .filter_map(|(sequence, signed)| Some((sequence, *signed.take()?)))
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
            received: Vec::new(),
        }
    }

    /// Every version the folder keeps, by sequence and [`crate::copy_id`], to compare with
    /// what was saved.
    pub fn stored_ids(&self) -> Vec<(u64, EntryId)> {
        let mut ids: Vec<(u64, EntryId)> = self
            .entries
            .values()
            .flat_map(Entry::stored)
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
        let mut folder = Folder::new(
            header.id,
            header.device,
            header.epoch.max(1),
            header.retention,
        );
        folder.epoch = header.epoch;
        folder.sequence = header.sequence;
        let versions = versions.into_iter();
        folder.entries.reserve(versions.size_hint().0);
        // Each entry's first version is kept compactly at once, so a large folder is never in
        // memory in full (ADR 0035).
        let mut several = Vec::new();
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
            let id = stored.version.entry;
            match folder.entries.get_mut(&id) {
                Some(entry) => {
                    several.push(id);
                    entry.conflicts.push(stored);
                }
                None => folder.entries.insert(Entry {
                    current: Slim::new(stored, &mut folder.devices),
                    conflicts: Conflicts::default(),
                }),
            }
        }
        // An entry with several versions: the winner is current, the rest in sequence order.
        several.sort_unstable();
        several.dedup();
        for id in several {
            let Some(entry) = folder.entries.get(&id).cloned() else {
                continue;
            };
            let mut versions = entry.conflicts.into_vec();
            versions.push(entry.current.into_stored(folder.id, &folder.devices));
            let win = winner(versions.iter().map(|v| &v.version));
            let current = versions.swap_remove(win);
            versions.sort_by_key(|v| v.sequence);
            let current = Slim::new(current, &mut folder.devices);
            folder.entries.insert(Entry {
                current,
                conflicts: versions.into(),
            });
        }
        let mut recycle = RecycleBin::new(header.retention);
        for item in recycled {
            recycle.insert(item);
        }
        folder.recycle = recycle;
        Ok(folder)
    }
}
