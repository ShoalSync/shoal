//! Current versions as a folder keeps them in memory (ADR 0035): every live entry has one, so
//! they are most of a large folder's memory. The folder ID is the folder's own, devices are
//! numbered once per folder, and a vector of one device's changes is two numbers.

use std::collections::HashMap;

use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::{DeviceId, EntryId, FolderId, Signed, VersionVector};

use crate::folder::StoredVersion;

/// The devices a folder's versions name, numbered in the order first seen.
#[derive(Debug, Clone, Default)]
pub(crate) struct Devices {
    ids: Vec<DeviceId>,
    numbers: HashMap<DeviceId, u32>,
}

impl Devices {
    fn number(&mut self, device: DeviceId) -> u32 {
        *self.numbers.entry(device).or_insert_with(|| {
            self.ids.push(device);
            u32::try_from(self.ids.len() - 1).expect("fewer than 2^32 devices")
        })
    }

    fn get(&self, number: u32) -> DeviceId {
        self.ids[number as usize]
    }
}

/// A version vector: one device's changes up to a counter, as nearly every current version's
/// is, or any other.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Vector {
    Single { device: u32, counter: u64 },
    Other(Box<VersionVector>),
}

/// An entry's current version, kept compactly.
#[derive(Debug, Clone)]
pub(crate) struct Slim {
    pub(crate) entry: EntryId,
    pub(crate) path: Box<str>,
    pub(crate) kind: EntryKind,
    pub(crate) modified_ms: i64,
    author: u32,
    vector: Vector,
    /// [`crate::copy_id`] of the version as signed.
    pub(crate) id: EntryId,
    /// The version as signed, until it is saved.
    pub(crate) signed: Option<Box<Signed<EntryVersion>>>,
    pub(crate) sequence: u64,
}

impl Slim {
    /// `stored`, compactly.
    pub(crate) fn new(stored: StoredVersion, devices: &mut Devices) -> Self {
        #[cfg(debug_assertions)]
        let full = stored.version.clone();
        let slim = Self::compact(stored, devices);
        // Development builds check that nothing is lost: versions are signed, so the one
        // handed out again must be the very one stored.
        #[cfg(debug_assertions)]
        assert_eq!(slim.version(full.folder, devices), full);
        slim
    }

    fn compact(stored: StoredVersion, devices: &mut Devices) -> Self {
        let EntryVersion {
            folder: _,
            entry,
            path,
            kind,
            modified_ms,
            author,
            version,
        } = stored.version;
        let vector = match version.as_single() {
            Some((device, counter)) => Vector::Single {
                device: devices.number(device),
                counter,
            },
            None => Vector::Other(Box::new(version)),
        };
        Self {
            entry,
            path: path.into_boxed_str(),
            kind,
            modified_ms,
            author: devices.number(author),
            vector,
            id: stored.id,
            signed: stored.signed,
            sequence: stored.sequence,
        }
    }

    pub(crate) fn author(&self, devices: &Devices) -> DeviceId {
        devices.get(self.author)
    }

    pub(crate) fn vector(&self, devices: &Devices) -> VersionVector {
        match &self.vector {
            Vector::Single { device, counter } => {
                VersionVector::single(devices.get(*device), *counter)
            }
            Vector::Other(vector) => (**vector).clone(),
        }
    }

    pub(crate) fn version(&self, folder: FolderId, devices: &Devices) -> EntryVersion {
        EntryVersion {
            folder,
            entry: self.entry,
            path: self.path.clone().into_string(),
            kind: self.kind.clone(),
            modified_ms: self.modified_ms,
            author: self.author(devices),
            version: self.vector(devices),
        }
    }

    /// The version in full, as stored.
    pub(crate) fn into_stored(self, folder: FolderId, devices: &Devices) -> StoredVersion {
        StoredVersion {
            version: self.version(folder, devices),
            id: self.id,
            signed: self.signed,
            sequence: self.sequence,
        }
    }
}

/// An entry's current version, as a folder hands it out: its parts read in place, and its
/// author and vector made on request.
#[derive(Debug, Clone, Copy)]
pub struct Current<'a> {
    /// The entry.
    pub entry: EntryId,
    /// Its path.
    pub path: &'a str,
    /// What it is.
    pub kind: &'a EntryKind,
    /// Its modification time, milliseconds since the Unix epoch.
    pub modified_ms: i64,
    slim: &'a Slim,
    folder: FolderId,
    devices: &'a Devices,
}

impl<'a> Current<'a> {
    pub(crate) fn new(slim: &'a Slim, folder: FolderId, devices: &'a Devices) -> Self {
        Self {
            entry: slim.entry,
            path: &slim.path,
            kind: &slim.kind,
            modified_ms: slim.modified_ms,
            slim,
            folder,
            devices,
        }
    }

    /// The device that made this version.
    pub fn author(&self) -> DeviceId {
        self.slim.author(self.devices)
    }

    /// The version's vector.
    pub fn vector(&self) -> VersionVector {
        self.slim.vector(self.devices)
    }

    /// The version in full.
    pub fn to_version(&self) -> EntryVersion {
        self.slim.version(self.folder, self.devices)
    }

    /// The local index sequence this replica stored the version at.
    pub fn sequence(&self) -> u64 {
        self.slim.sequence
    }
}

impl PartialEq for Current<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.to_version() == other.to_version()
    }
}

impl PartialEq<EntryVersion> for Current<'_> {
    fn eq(&self, other: &EntryVersion) -> bool {
        self.to_version() == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidefell_proto::ContentHash;

    fn stored(version: VersionVector) -> StoredVersion {
        StoredVersion {
            version: EntryVersion {
                folder: FolderId([1; 32]),
                entry: EntryId([2; 16]),
                path: "a/b.txt".into(),
                kind: EntryKind::File {
                    content: ContentHash([3; 32]),
                    size: 4,
                    executable: false,
                },
                modified_ms: 5,
                author: DeviceId([6; 32]),
                version,
            },
            id: EntryId([7; 16]),
            signed: None,
            sequence: 8,
        }
    }

    #[test]
    fn versions_come_back_exactly() {
        let (a, b) = (DeviceId([6; 32]), DeviceId([9; 32]));
        let mut two = VersionVector::single(a, 3);
        two.add(b, 1);
        let mut holes = VersionVector::single(a, 3);
        holes.remove(a, 2);
        for vector in [
            VersionVector::single(a, 3),
            two,
            holes,
            VersionVector::new(),
        ] {
            let mut devices = Devices::default();
            let stored = stored(vector);
            let slim = Slim::new(stored.clone(), &mut devices);
            assert_eq!(slim.into_stored(FolderId([1; 32]), &devices), stored);
        }
    }

    #[test]
    fn a_current_version_is_small() {
        assert!(
            std::mem::size_of::<Slim>() <= 144,
            "{}",
            std::mem::size_of::<Slim>()
        );
    }
}
