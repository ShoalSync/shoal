//! The files on disk a folder has recorded (ADR 0035): one per file, so kept densely. Each
//! sits beside its path in one vector, found through tables of positions by path and by
//! content, and holds its base vector as two numbers when, as nearly always, one device made
//! all of it.

use std::collections::hash_map::RandomState;
use std::collections::{BTreeSet, HashMap};
use std::hash::BuildHasher;
use std::sync::Arc;

use hashbrown::HashTable;
use tidefell_proto::{ContentHash, DeviceId, EntryId, VersionVector};

use crate::state::LocalFile;
use crate::tracked::encode;

/// The devices the recorded bases name, numbered in the order first seen.
#[derive(Debug, Clone, Default)]
struct Devices {
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

    fn get(&self, number: u32) -> Option<DeviceId> {
        self.ids.get(number as usize).copied()
    }
}

/// A base vector: none, one device's changes up to a counter, or any other.
#[derive(Debug, Clone)]
enum Base {
    Empty,
    Single { device: u32, counter: u64 },
    Other(Box<VersionVector>),
}

impl Base {
    fn new(vector: VersionVector, devices: &mut Devices) -> Self {
        if vector.is_empty() {
            return Self::Empty;
        }
        match vector.as_single() {
            Some((device, counter)) => Self::Single {
                device: devices.number(device),
                counter,
            },
            None => Self::Other(Box::new(vector)),
        }
    }

    fn vector(&self, devices: &Devices) -> VersionVector {
        match self {
            Self::Empty => VersionVector::new(),
            Self::Single { device, counter } => devices
                .get(*device)
                .map_or_else(VersionVector::new, |d| VersionVector::single(d, *counter)),
            Self::Other(vector) => (**vector).clone(),
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Self::Empty)
    }
}

/// A recorded file, kept compactly.
#[derive(Debug, Clone)]
struct Compact {
    entry: EntryId,
    content: ContentHash,
    size: u64,
    /// Nanoseconds since the Unix epoch: a time past 2262 is kept as the latest before it, so
    /// such a file is read again at each scan.
    mtime_ns: i64,
    base: Base,
    executable: bool,
}

/// A file as recorded, read in place: its base vector is made on request.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Local<'a> {
    pub(crate) entry: EntryId,
    pub(crate) content: ContentHash,
    pub(crate) size: u64,
    pub(crate) mtime_ns: i128,
    pub(crate) executable: bool,
    base: &'a Base,
    devices: &'a Devices,
}

impl Local<'_> {
    /// The version vector the file's content corresponds to (ADR 0024).
    pub(crate) fn base(self) -> VersionVector {
        self.base.vector(self.devices)
    }

    /// True if the base is unknown, as for files recorded before it was kept.
    pub(crate) fn base_is_empty(self) -> bool {
        self.base.is_empty()
    }

    /// The record in full.
    pub(crate) fn to_file(self) -> LocalFile {
        LocalFile {
            entry: self.entry,
            content: self.content,
            size: self.size,
            mtime_ns: self.mtime_ns,
            executable: self.executable,
            base: self.base(),
        }
    }
}

type Slot = Option<(Arc<str>, Compact)>;

/// Files on disk by path, also indexed by content so chunks can be read from them instead of
/// being kept a second time in the block store (ADR 0027). A forgotten file's place is reused.
#[derive(Debug, Clone, Default)]
pub(crate) struct LocalFiles {
    slots: Vec<Slot>,
    /// Places in `slots` left by forgotten files.
    free: Vec<u32>,
    by_path: HashTable<u32>,
    /// Several files can have one content, so this has a place for each.
    by_content: HashTable<u32>,
    /// Likewise by entry: a file renamed elsewhere can be at its old and new paths a while.
    by_entry: HashTable<u32>,
    hasher: RandomState,
    devices: Devices,
    /// Paths recorded or forgotten since last saved (ADR 0034).
    changed: BTreeSet<Arc<str>>,
}

impl LocalFiles {
    fn slot(&self, slot: u32) -> Option<&(Arc<str>, Compact)> {
        self.slots.get(slot as usize)?.as_ref()
    }

    fn view<'a>(&'a self, file: &'a Compact) -> Local<'a> {
        Local {
            entry: file.entry,
            content: file.content,
            size: file.size,
            mtime_ns: i128::from(file.mtime_ns),
            executable: file.executable,
            base: &file.base,
            devices: &self.devices,
        }
    }

    fn compact(&mut self, file: LocalFile) -> Compact {
        let mtime_ns = i64::try_from(file.mtime_ns).unwrap_or(if file.mtime_ns < 0 {
            i64::MIN
        } else {
            i64::MAX
        });
        Compact {
            entry: file.entry,
            content: file.content,
            size: file.size,
            mtime_ns,
            base: Base::new(file.base, &mut self.devices),
            executable: file.executable,
        }
    }

    fn find(&self, path: &str) -> Option<u32> {
        let hash = self.hasher.hash_one(path);
        self.by_path
            .find(hash, |&s| self.slot(s).is_some_and(|(p, _)| &**p == path))
            .copied()
    }

    /// Records the file at `path`, replacing what was recorded there.
    pub(crate) fn insert(&mut self, path: String, file: LocalFile) -> Option<LocalFile> {
        self.insert_shared(path, file, None)
    }

    /// Records the file at `path` as [`LocalFiles::insert`] does, keeping `shared` as its path
    /// if that is the same path: the copy its entry's version holds (ADR 0035).
    pub(crate) fn insert_shared(
        &mut self,
        path: String,
        file: LocalFile,
        shared: Option<Arc<str>>,
    ) -> Option<LocalFile> {
        let shared = shared.filter(|s| **s == *path);
        let file = self.compact(file);
        if let Some(slot) = self.find(&path) {
            let (kept, old) = self.slots.get_mut(slot as usize)?.as_mut()?;
            if let Some(shared) = shared {
                *kept = shared;
            }
            let kept = kept.clone();
            let old = std::mem::replace(old, file);
            self.changed.insert(kept);
            if Some(old.content) != self.slot(slot).map(|(_, f)| f.content) {
                self.unindex_content(slot, &old.content);
                self.index_content(slot);
            }
            if Some(old.entry) != self.slot(slot).map(|(_, f)| f.entry) {
                self.unindex_entry(slot, &old.entry);
                self.index_entry(slot);
            }
            return Some(self.view(&old).to_file());
        }
        let path: Arc<str> = shared.unwrap_or_else(|| Arc::from(path));
        self.changed.insert(path.clone());
        let hash = self.hasher.hash_one(&*path);
        let slot = match self.free.pop() {
            Some(slot) => {
                if let Some(place) = self.slots.get_mut(slot as usize) {
                    *place = Some((path, file));
                }
                slot
            }
            None => {
                self.slots.push(Some((path, file)));
                u32::try_from(self.slots.len() - 1).expect("fewer than 2^32 files")
            }
        };
        let Self {
            slots,
            by_path,
            hasher,
            ..
        } = self;
        by_path.insert_unique(hash, slot, |&s| path_hash(hasher, slots, s));
        self.index_content(slot);
        self.index_entry(slot);
        None
    }

    /// Forgets the file at `path`.
    pub(crate) fn remove(&mut self, path: &str) -> Option<LocalFile> {
        let slot = self.find(path)?;
        let hash = self.hasher.hash_one(path);
        if let Ok(found) = self.by_path.find_entry(hash, |&s| s == slot) {
            found.remove();
        }
        let (content, entry) = self.slot(slot).map(|(_, f)| (f.content, f.entry))?;
        self.unindex_content(slot, &content);
        self.unindex_entry(slot, &entry);
        let (kept, old) = self.slots.get_mut(slot as usize)?.take()?;
        self.free.push(slot);
        self.changed.insert(kept);
        Some(self.view(&old).to_file())
    }

    fn index_content(&mut self, slot: u32) {
        let Some(content) = self.slot(slot).map(|(_, f)| f.content) else {
            return;
        };
        let hash = self.hasher.hash_one(content);
        let Self {
            slots,
            by_content,
            hasher,
            ..
        } = self;
        by_content.insert_unique(hash, slot, |&s| content_hash(hasher, slots, s));
    }

    fn index_entry(&mut self, slot: u32) {
        let Some(entry) = self.slot(slot).map(|(_, f)| f.entry) else {
            return;
        };
        let hash = self.hasher.hash_one(entry);
        let Self {
            slots,
            by_entry,
            hasher,
            ..
        } = self;
        by_entry.insert_unique(hash, slot, |&s| entry_hash(hasher, slots, s));
    }

    fn unindex_entry(&mut self, slot: u32, entry: &EntryId) {
        let hash = self.hasher.hash_one(entry);
        if let Ok(found) = self.by_entry.find_entry(hash, |&s| s == slot) {
            found.remove();
        }
    }

    fn unindex_content(&mut self, slot: u32, content: &ContentHash) {
        let hash = self.hasher.hash_one(content);
        if let Ok(found) = self.by_content.find_entry(hash, |&s| s == slot) {
            found.remove();
        }
    }

    /// The paths recorded or forgotten since last asked, encoded for the database.
    pub(crate) fn take_changed(&mut self) -> Vec<crate::tracked::Changed> {
        std::mem::take(&mut self.changed)
            .into_iter()
            .map(|path| {
                let file = self.get(&path).map(|f| encode(&f.to_file()));
                (encode(&&*path), file)
            })
            .collect()
    }

    /// Every file, encoded for the database.
    pub(crate) fn all(&self) -> Vec<crate::tracked::Changed> {
        self.iter()
            .map(|(p, f)| (encode(&&**p), Some(encode(&f.to_file()))))
            .collect()
    }

    /// Loads what [`LocalFiles::all`] and [`LocalFiles::take_changed`] wrote.
    /// `shared` gives the path an entry's version holds, to keep instead of a copy.
    pub(crate) fn decode(
        records: Vec<(Vec<u8>, Vec<u8>)>,
        shared: impl Fn(&EntryId) -> Option<Arc<str>>,
    ) -> std::result::Result<Self, postcard::Error> {
        let mut files = LocalFiles::default();
        files.reserve(records.len());
        for (path, file) in records {
            let file: LocalFile = postcard::from_bytes(&file)?;
            let shared = shared(&file.entry);
            files.insert_shared(postcard::from_bytes(&path)?, file, shared);
        }
        files.changed.clear();
        Ok(files)
    }

    fn reserve(&mut self, more: usize) {
        self.slots.reserve_exact(more);
        let Self {
            slots,
            by_path,
            by_content,
            by_entry,
            hasher,
            ..
        } = self;
        by_path.reserve(more, |&s| path_hash(hasher, slots, s));
        by_content.reserve(more, |&s| content_hash(hasher, slots, s));
        by_entry.reserve(more, |&s| entry_hash(hasher, slots, s));
    }

    /// The file recorded at `path`.
    pub(crate) fn get(&self, path: &str) -> Option<Local<'_>> {
        self.slot(self.find(path)?).map(|(_, f)| self.view(f))
    }

    /// True if a file is recorded at `path`.
    pub(crate) fn contains_key(&self, path: &str) -> bool {
        self.find(path).is_some()
    }

    /// Paths of the files on disk with `content`.
    pub(crate) fn paths_with(&self, content: &ContentHash) -> impl Iterator<Item = &str> {
        let content = *content;
        let hash = self.hasher.hash_one(content);
        self.by_content
            .iter_hash(hash)
            .filter_map(|&s| self.slot(s))
            .filter(move |(_, f)| f.content == content)
            .map(|(p, _)| &**p)
    }

    /// The files recorded as `entry`, with their paths: nearly always one.
    pub(crate) fn with_entry(&self, entry: &EntryId) -> impl Iterator<Item = (&str, Local<'_>)> {
        let entry = *entry;
        let hash = self.hasher.hash_one(entry);
        self.by_entry
            .iter_hash(hash)
            .filter_map(|&s| self.slot(s))
            .filter(move |(_, f)| f.entry == entry)
            .map(|(p, f)| (&**p, self.view(f)))
    }

    /// Every file and its path, in no particular order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Arc<str>, Local<'_>)> {
        self.slots.iter().flatten().map(|(p, f)| (p, self.view(f)))
    }

    /// Every path, in no particular order.
    pub(crate) fn keys(&self) -> impl Iterator<Item = &Arc<str>> {
        self.slots.iter().flatten().map(|(p, _)| p)
    }

    /// Every file, in no particular order.
    pub(crate) fn values(&self) -> impl Iterator<Item = Local<'_>> {
        self.iter().map(|(_, f)| f)
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn path_hash(hasher: &RandomState, slots: &[Slot], slot: u32) -> u64 {
    match slots.get(slot as usize) {
        Some(Some((path, _))) => hasher.hash_one(&**path),
        _ => 0,
    }
}

fn entry_hash(hasher: &RandomState, slots: &[Slot], slot: u32) -> u64 {
    match slots.get(slot as usize) {
        Some(Some((_, file))) => hasher.hash_one(file.entry),
        _ => 0,
    }
}

fn content_hash(hasher: &RandomState, slots: &[Slot], slot: u32) -> u64 {
    match slots.get(slot as usize) {
        Some(Some((_, file))) => hasher.hash_one(file.content),
        _ => 0,
    }
}

impl FromIterator<(String, LocalFile)> for LocalFiles {
    fn from_iter<I: IntoIterator<Item = (String, LocalFile)>>(iter: I) -> Self {
        let mut out = Self::default();
        for (path, file) in iter {
            out.insert(path, file);
        }
        out.changed.clear();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(n: u8, base: VersionVector) -> LocalFile {
        LocalFile {
            entry: EntryId([n; 16]),
            content: ContentHash([n % 2; 32]),
            size: u64::from(n),
            mtime_ns: i128::from(n) * 1_000_000_007,
            executable: n.is_multiple_of(3),
            base,
        }
    }

    #[test]
    fn files_come_back_exactly() {
        let (a, b) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let mut two = VersionVector::single(a, 3);
        two.add(b, 1);
        let mut holes = VersionVector::single(a, 3);
        holes.remove(a, 2);
        let bases = [
            VersionVector::new(),
            VersionVector::single(b, 9),
            two,
            holes,
        ];
        let mut files = LocalFiles::default();
        for (n, base) in bases.iter().enumerate() {
            let n = u8::try_from(n).unwrap();
            files.insert(format!("f{n}"), file(n, base.clone()));
        }
        for (n, base) in bases.iter().enumerate() {
            let n = u8::try_from(n).unwrap();
            assert_eq!(
                files.get(&format!("f{n}")).unwrap().to_file(),
                file(n, base.clone())
            );
        }
        let mut with: Vec<&str> = files.paths_with(&ContentHash([0; 32])).collect();
        with.sort_unstable();
        assert_eq!(with, ["f0", "f2"]);
        // A forgotten file's place is reused, and the indexes follow.
        assert_eq!(files.remove("f0"), Some(file(0, bases[0].clone())));
        files.insert("g".into(), file(5, VersionVector::new()));
        assert_eq!(files.len(), 4);
        let with: Vec<&str> = files.paths_with(&ContentHash([0; 32])).collect();
        assert_eq!(with, ["f2"]);
        assert!(files.get("f0").is_none());
        assert_eq!(files.with_entry(&EntryId([0; 16])).count(), 0);
        let renamed: Vec<&str> = files
            .with_entry(&EntryId([5; 16]))
            .map(|(p, _)| p)
            .collect();
        assert_eq!(renamed, ["g"]);
        assert_eq!(files.get("g").unwrap().entry, EntryId([5; 16]));
    }
}
