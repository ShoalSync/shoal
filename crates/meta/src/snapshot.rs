//! On-disk snapshots of a [`Folder`].
//!
//! Format 2: the bytes `tidefell-meta 2\n` followed by the postcard encoding of [`Snapshot`].
//! Versions are stored in signed form and re-verified on load. Format 1 differs only in how
//! recycle-bin items encode their version vectors, from before vectors had holes.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tidefell_proto::sync::{EntryVersion, LegacyEntryVersion};
use tidefell_proto::{DeviceId, EntryId, FolderId, Signed};

use crate::folder::{Entry, StoredVersion};
use crate::recycle::{RecycleBin, Recycled, Retention};
use crate::{Error, Folder, Result};

/// Header identifying the snapshot format.
pub const SNAPSHOT_HEADER: &[u8] = b"tidefell-meta 2\n";

/// Header of format 1.
const SNAPSHOT_HEADER_V1: &[u8] = b"tidefell-meta 1\n";

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

#[derive(Deserialize)]
struct SnapshotV1 {
    id: FolderId,
    device: DeviceId,
    epoch: u64,
    sequence: u64,
    retention: Retention,
    versions: Vec<(bool, u64, Signed<EntryVersion>)>,
    recycled: Vec<(LegacyEntryVersion, i64)>,
}

impl From<SnapshotV1> for Snapshot {
    fn from(v1: SnapshotV1) -> Self {
        Self {
            id: v1.id,
            device: v1.device,
            epoch: v1.epoch,
            sequence: v1.sequence,
            retention: v1.retention,
            versions: v1.versions,
            recycled: v1
                .recycled
                .into_iter()
                .map(|(version, deleted_at_ms)| Recycled {
                    version: version.into(),
                    deleted_at_ms,
                    conflict_of: None,
                })
                .collect(),
        }
    }
}

impl Folder {
    /// Encodes the folder as a snapshot.
    pub fn to_snapshot(&self) -> Vec<u8> {
        let mut versions = Vec::new();
        for entry in self.entries.values() {
            // Only versions not yet saved elsewhere are held signed (ADR 0035); a snapshot is
            // made of a folder that has them all, such as one read from a snapshot.
            if let Some(signed) = &entry.current.signed {
                versions.push((true, entry.current.sequence, (**signed).clone()));
            }
            for c in entry.conflicts.iter() {
                if let Some(signed) = &c.signed {
                    versions.push((false, c.sequence, (**signed).clone()));
                }
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
        let snapshot: Snapshot = if let Some(body) = bytes.strip_prefix(SNAPSHOT_HEADER) {
            postcard::from_bytes(body).map_err(Error::Snapshot)?
        } else if let Some(body) = bytes.strip_prefix(SNAPSHOT_HEADER_V1) {
            postcard::from_bytes::<SnapshotV1>(body)
                .map_err(Error::Snapshot)?
                .into()
        } else {
            return Err(Error::UnsupportedFormat);
        };

        let mut currents: HashMap<EntryId, StoredVersion> = HashMap::new();
        let mut conflicts: HashMap<EntryId, Vec<StoredVersion>> = HashMap::new();
        for (is_current, sequence, signed) in snapshot.versions {
            let version = signed.verify_authored()?;
            if version.folder != snapshot.id {
                return Err(Error::WrongFolder);
            }
            let entry = version.entry;
            let stored = StoredVersion::new(version, signed, sequence);
            if is_current {
                currents.insert(entry, stored);
            } else {
                conflicts.entry(entry).or_default().push(stored);
            }
        }
        let mut entries = crate::entries::Entries::default();
        let mut devices = crate::compact::Devices::default();
        for (id, current) in currents {
            let conflicts = conflicts.remove(&id).unwrap_or_default();
            let current = crate::compact::Slim::new(current, &mut devices);
            entries.insert(Entry {
                current,
                conflicts: conflicts.into(),
            });
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
            devices,
            recycle,
            layout: std::sync::OnceLock::new(),
            unfolded: std::sync::OnceLock::new(),
            folding: crate::Folding::None,
            pending: crate::journal::Pending::default(),
            changes: std::cell::RefCell::default(),
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
        // So the rename survives a power cut (ADR 0034); some filesystems cannot sync a
        // directory, and keep the rename without that guarantee.
        #[cfg(unix)]
        if let Some(dir) = path.parent() {
            let _ = fs::File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    }

    /// Loads a snapshot written by [`Folder::save`].
    pub fn load(path: &Path) -> Result<Self> {
        Self::from_snapshot(&fs::read(path)?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ed25519_dalek::SigningKey;
    use tidefell_proto::sync::EntryKind;
    use tidefell_proto::{ContentHash, EntryId};

    use super::*;

    /// A recycle-bin item as format 1 encoded it.
    #[derive(Serialize)]
    struct RecycledV1 {
        folder: FolderId,
        entry: EntryId,
        path: String,
        kind: EntryKind,
        modified_ms: i64,
        author: DeviceId,
        version: BTreeMap<DeviceId, u64>,
        deleted_at_ms: i64,
    }

    #[derive(Serialize)]
    struct SnapshotV1Out {
        id: FolderId,
        device: DeviceId,
        epoch: u64,
        sequence: u64,
        retention: Retention,
        versions: Vec<(bool, u64, Signed<EntryVersion>)>,
        recycled: Vec<RecycledV1>,
    }

    #[test]
    fn format_1_loads() {
        let key = SigningKey::from_bytes(&[2; 32]);
        let device = DeviceId(key.verifying_key().to_bytes());
        let mut folder = Folder::new(FolderId([9; 32]), device, 7, Retention::Days(30));
        let entry = EntryId([1; 16]);
        let file = EntryKind::File {
            content: ContentHash([3; 32]),
            size: 1,
            executable: false,
        };
        folder.local_change(&key, entry, "f", file, 1).unwrap();
        folder
            .local_change(&key, entry, "f", EntryKind::Tombstone, 2)
            .unwrap();
        let current: Snapshot =
            postcard::from_bytes(&folder.to_snapshot()[SNAPSHOT_HEADER.len()..]).unwrap();
        let v1 = SnapshotV1Out {
            id: current.id,
            device: current.device,
            epoch: current.epoch,
            sequence: current.sequence,
            retention: current.retention,
            versions: current.versions,
            recycled: current
                .recycled
                .into_iter()
                .map(|r| RecycledV1 {
                    folder: r.version.folder,
                    entry: r.version.entry,
                    path: r.version.path,
                    kind: r.version.kind,
                    modified_ms: r.version.modified_ms,
                    author: r.version.author,
                    version: r.version.version.iter().map(|(d, c)| (*d, *c)).collect(),
                    deleted_at_ms: r.deleted_at_ms,
                })
                .collect(),
        };
        let mut bytes = SNAPSHOT_HEADER_V1.to_vec();
        bytes.extend(postcard::to_stdvec(&v1).unwrap());
        let loaded = Folder::from_snapshot(&bytes).unwrap();
        assert_eq!(loaded.recycle_bin(), folder.recycle_bin());
        assert_eq!(loaded.current(&entry), folder.current(&entry));
    }
}
