//! On-disk snapshots of a [`Folder`].
//!
//! Format 1: the bytes `tidefell-meta 1\n` followed by the postcard encoding of [`Snapshot`].
//! Versions are stored in signed form and re-verified on load.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tidefell_proto::sync::EntryVersion;
use tidefell_proto::{DeviceId, EntryId, FolderId, Signed};

use crate::folder::{Entry, StoredVersion};
use crate::recycle::{RecycleBin, Recycled, Retention};
use crate::{Error, Folder, Result};

/// Header identifying the snapshot format.
pub const SNAPSHOT_HEADER: &[u8] = b"tidefell-meta 1\n";

#[derive(Serialize, Deserialize)]
struct Snapshot {
    id: FolderId,
    device: DeviceId,
    epoch: u64,
    sequence: u64,
    retention: Retention,
    /// Every stored version: (entry is current, sequence, signed version).
    versions: Vec<(bool, u64, Signed<EntryVersion>)>,
    recycled: Vec<Recycled>,
}

impl Folder {
    /// Encodes the folder as a snapshot.
    pub fn to_snapshot(&self) -> Vec<u8> {
        let mut versions = Vec::new();
        for entry in self.entries.values() {
            versions.push((true, entry.current.sequence, entry.current.signed.clone()));
            for c in &entry.conflicts {
                versions.push((false, c.sequence, c.signed.clone()));
            }
        }
        versions.sort_by_key(|(_, seq, _)| *seq);
        let snapshot = Snapshot {
            id: self.id,
            device: self.device,
            epoch: self.epoch,
            sequence: self.sequence,
            retention: self.recycle.retention(),
            versions,
            recycled: self.recycle.iter().cloned().collect(),
        };
        let mut out = SNAPSHOT_HEADER.to_vec();
        out.extend(postcard::to_stdvec(&snapshot).expect("in-memory encoding cannot fail"));
        out
    }

    /// Decodes a snapshot, re-verifying every version.
    pub fn from_snapshot(bytes: &[u8]) -> Result<Self> {
        let body = bytes
            .strip_prefix(SNAPSHOT_HEADER)
            .ok_or(Error::UnsupportedFormat)?;
        let snapshot: Snapshot = postcard::from_bytes(body).map_err(Error::Snapshot)?;

        let mut currents: HashMap<EntryId, StoredVersion> = HashMap::new();
        let mut conflicts: HashMap<EntryId, Vec<StoredVersion>> = HashMap::new();
        for (is_current, sequence, signed) in snapshot.versions {
            let version = signed.verify_authored()?;
            if version.folder != snapshot.id {
                return Err(Error::WrongFolder);
            }
            let entry = version.entry;
            let stored = StoredVersion {
                version,
                signed,
                sequence,
            };
            if is_current {
                currents.insert(entry, stored);
            } else {
                conflicts.entry(entry).or_default().push(stored);
            }
        }
        let mut entries = HashMap::new();
        for (id, current) in currents {
            let conflicts = conflicts.remove(&id).unwrap_or_default();
            entries.insert(id, Entry { current, conflicts });
        }
        if !conflicts.is_empty() {
            return Err(Error::UnsupportedFormat);
        }

        let mut recycle = RecycleBin::new(snapshot.retention);
        for item in snapshot.recycled {
            recycle.insert(item);
        }
        Ok(Self {
            id: snapshot.id,
            device: snapshot.device,
            epoch: snapshot.epoch,
            sequence: snapshot.sequence,
            entries,
            recycle,
        })
    }

    /// Writes a snapshot to `path` atomically (write to a temporary file, then rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("tmp");
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&self.to_snapshot())?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Loads a snapshot written by [`Folder::save`].
    pub fn load(path: &Path) -> Result<Self> {
        Self::from_snapshot(&fs::read(path)?)
    }
}
