//! Where each live entry is written on disk when several claim the same path (ADR 0024,
//! "Entries at the same path" in `specs/sync-v1.md`).
//!
//! Every replica computes the same layout from the same entries, so no versions are made to
//! resolve a clash. A device whose filesystem matches names without regard to case lays them
//! out by their case-folded names instead (ADR 0033): only where that device writes changes.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::EntryId;

/// How a device's filesystem tells names apart (ADR 0033).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Folding {
    /// Names that differ in any way are different files.
    #[default]
    None,
    /// Names that differ only in case are one file. Case is folded with Unicode's lowercase
    /// mapping, which approximates what FAT, NTFS, APFS and Android's shared storage do.
    Case,
}

impl Folding {
    /// `path` as this device compares names.
    pub fn fold<'a>(self, path: &'a str) -> Cow<'a, str> {
        match self {
            Self::Case if path.chars().any(char::is_uppercase) => Cow::Owned(path.to_lowercase()),
            _ => Cow::Borrowed(path),
        }
    }
}

/// The on-disk path of every live entry.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    paths: HashMap<EntryId, String>,
    /// By folded path.
    entries: HashMap<String, EntryId>,
    folding: Folding,
}

impl Layout {
    /// Lays out the current versions of live entries. Tombstones are ignored.
    pub fn new<'a>(versions: impl IntoIterator<Item = &'a EntryVersion>) -> Self {
        Self::folded(versions, Folding::None)
    }

    /// Lays out the current versions of live entries for a device that folds names with
    /// `folding`: entries whose paths fold to one claim one place, and folders whose paths fold
    /// to one are one folder, spelled as the lowest entry ID beneath it spells it.
    pub fn folded<'a>(
        versions: impl IntoIterator<Item = &'a EntryVersion>,
        folding: Folding,
    ) -> Self {
        let mut live: Vec<(EntryId, Cow<'a, str>, bool)> = versions
            .into_iter()
            .filter(|v| !matches!(v.kind, EntryKind::Tombstone))
            .map(|v| {
                (
                    v.entry,
                    Cow::Borrowed(v.path.as_str()),
                    matches!(v.kind, EntryKind::Directory),
                )
            })
            .collect();
        if folding != Folding::None {
            respell_folders(&mut live, folding);
        }
        let fold = |path: &str| folding.fold(path).into_owned();

        // Paths that are directories, folded: explicit ones and the ancestors of every entry.
        let mut dirs: BTreeSet<String> = BTreeSet::new();
        for (_, path, directory) in &live {
            if *directory {
                dirs.insert(fold(path));
            }
            let mut rest: &str = path;
            while let Some((parent, _)) = rest.rsplit_once('/') {
                dirs.insert(fold(parent));
                rest = parent;
            }
        }

        // Files and symlinks, grouped by folded path, lowest entry ID first, each with the
        // path it would keep.
        let mut claims: BTreeMap<String, Vec<(EntryId, &str)>> = BTreeMap::new();
        let mut layout = Self {
            folding,
            ..Self::default()
        };
        for (entry, path, directory) in &live {
            if *directory {
                layout.paths.insert(*entry, path.to_string());
                let lowest = layout.entries.entry(fold(path)).or_insert(*entry);
                *lowest = (*lowest).min(*entry);
            } else {
                claims.entry(fold(path)).or_default().push((*entry, path));
            }
        }

        let mut taken: BTreeSet<String> = dirs.clone();
        let mut displaced: Vec<(EntryId, &str)> = Vec::new();
        for (folded, mut ids) in claims {
            ids.sort();
            let keeps = if dirs.contains(&folded) { 0 } else { 1 };
            for (i, (id, path)) in ids.into_iter().enumerate() {
                if i < keeps {
                    layout.place(id, path.to_owned());
                    taken.insert(folded.clone());
                } else {
                    displaced.push((id, path));
                }
            }
        }
        displaced.sort();
        for (id, path) in displaced {
            let mut name = conflict_name(path, id, 8);
            if taken.contains(&fold(&name)) {
                name = conflict_name(path, id, 32);
            }
            taken.insert(fold(&name));
            layout.place(id, name);
        }
        layout
    }

    fn place(&mut self, entry: EntryId, path: String) {
        self.entries
            .insert(self.folding.fold(&path).into_owned(), entry);
        self.paths.insert(entry, path);
    }

    /// Where `entry` is on disk, if it is live.
    pub fn path_of(&self, entry: &EntryId) -> Option<&str> {
        self.paths.get(entry).map(String::as_str)
    }

    /// The live entry on disk at `path`, as this device compares names. For a directory claimed
    /// by several entries, the one with the lowest ID.
    pub fn entry_at(&self, path: &str) -> Option<EntryId> {
        self.entries.get(self.folding.fold(path).as_ref()).copied()
    }
}

/// Spells every folder whose path folds to the same one the same way: as the entry with the
/// lowest ID beneath it, or the folder entry itself, spells it.
fn respell_folders(live: &mut [(EntryId, Cow<'_, str>, bool)], folding: Folding) {
    // For each folded folder path, the lowest entry beneath it and its spelling of the folder's
    // last name.
    let mut spelling: HashMap<String, (EntryId, String)> = HashMap::new();
    for (entry, path, directory) in live.iter() {
        let mut folders: Vec<&str> = Vec::new();
        if *directory {
            folders.push(path);
        }
        let mut rest: &str = path;
        while let Some((parent, _)) = rest.rsplit_once('/') {
            folders.push(parent);
            rest = parent;
        }
        for folder in folders {
            let name = folder.rsplit_once('/').map_or(folder, |(_, name)| name);
            let lowest = spelling
                .entry(folding.fold(folder).into_owned())
                .or_insert((*entry, name.to_owned()));
            if *entry < lowest.0 {
                *lowest = (*entry, name.to_owned());
            }
        }
    }
    for (_, path, directory) in live.iter_mut() {
        let names: Vec<&str> = path.split('/').collect();
        let folders = if *directory {
            names.len()
        } else {
            names.len().saturating_sub(1)
        };
        let mut respelled: Vec<String> = Vec::with_capacity(names.len());
        for (i, name) in names.iter().enumerate() {
            if i < folders {
                let prefix = names[..=i].join("/");
                let chosen = spelling
                    .get(folding.fold(&prefix).as_ref())
                    .map_or(*name, |(_, s)| s.as_str());
                respelled.push(chosen.to_owned());
            } else {
                respelled.push((*name).to_owned());
            }
        }
        let respelled = respelled.join("/");
        if respelled != *path {
            *path = Cow::Owned(respelled);
        }
    }
}

/// `dir/<stem> (conflict <id>)<extension>` for an entry that gives way at `path`, using the
/// first `hex_digits` hex digits of its ID.
fn conflict_name(path: &str, entry: EntryId, hex_digits: usize) -> String {
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (Some(dir), name),
        None => (None, path),
    };
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    let id = entry.to_string();
    let file = format!(
        "{stem} (conflict {}){extension}",
        &id[..hex_digits.min(id.len())]
    );
    match dir {
        Some(dir) => format!("{dir}/{file}"),
        None => file,
    }
}

#[cfg(test)]
mod tests {
    use tidefell_proto::{ContentHash, DeviceId, FolderId, VersionVector};

    use super::*;

    fn version(entry: u8, path: &str, kind: EntryKind) -> EntryVersion {
        EntryVersion {
            folder: FolderId([1; 32]),
            entry: EntryId([entry; 16]),
            path: path.into(),
            kind,
            modified_ms: 0,
            author: DeviceId([2; 32]),
            version: VersionVector::new(),
        }
    }

    fn file(entry: u8, path: &str) -> EntryVersion {
        version(
            entry,
            path,
            EntryKind::File {
                content: ContentHash([entry; 32]),
                size: 1,
                executable: false,
            },
        )
    }

    fn id(n: u8) -> EntryId {
        EntryId([n; 16])
    }

    #[test]
    fn lowest_entry_keeps_the_path() {
        let versions = [file(9, "notes.txt"), file(3, "notes.txt"), file(5, "other")];
        let layout = Layout::new(&versions);
        assert_eq!(layout.path_of(&id(3)), Some("notes.txt"));
        assert_eq!(
            layout.path_of(&id(9)),
            Some("notes (conflict 09090909).txt")
        );
        assert_eq!(layout.path_of(&id(5)), Some("other"));
        assert_eq!(layout.entry_at("notes.txt"), Some(id(3)));
        assert_eq!(
            layout.entry_at("notes (conflict 09090909).txt"),
            Some(id(9))
        );
    }

    #[test]
    fn names_that_fold_together_claim_one_place_on_a_case_insensitive_device() {
        let versions = [file(9, "Photo.jpg"), file(3, "photo.jpg"), file(5, "other")];
        // Every device that tells them apart keeps both names.
        let layout = Layout::new(&versions);
        assert_eq!(layout.path_of(&id(9)), Some("Photo.jpg"));
        assert_eq!(layout.path_of(&id(3)), Some("photo.jpg"));
        // A device that does not writes the higher ID under a conflict name.
        let layout = Layout::folded(&versions, Folding::Case);
        assert_eq!(layout.path_of(&id(3)), Some("photo.jpg"));
        assert_eq!(
            layout.path_of(&id(9)),
            Some("Photo (conflict 09090909).jpg")
        );
        assert_eq!(layout.entry_at("PHOTO.JPG"), Some(id(3)));
        assert_eq!(
            layout.entry_at("photo (conflict 09090909).jpg"),
            Some(id(9))
        );
        assert_eq!(layout.path_of(&id(5)), Some("other"));
    }

    #[test]
    fn folders_that_fold_together_are_one_spelled_as_the_lowest_entry_spells_it() {
        let versions = [
            file(7, "docs/b.txt"),
            file(4, "Docs/a.txt"),
            file(8, "DOCS/Sub/c.txt"),
            file(2, "docs/sub/d.txt"),
        ];
        let layout = Layout::folded(&versions, Folding::Case);
        assert_eq!(layout.path_of(&id(4)), Some("docs/a.txt"));
        assert_eq!(layout.path_of(&id(7)), Some("docs/b.txt"));
        assert_eq!(layout.path_of(&id(8)), Some("docs/sub/c.txt"));
        assert_eq!(layout.path_of(&id(2)), Some("docs/sub/d.txt"));
        // A file and a folder whose names fold together: the folder wins, as for equal names.
        let versions = [file(1, "notes"), file(2, "Notes/x.txt")];
        let layout = Layout::folded(&versions, Folding::Case);
        assert_eq!(layout.path_of(&id(2)), Some("Notes/x.txt"));
        assert_eq!(layout.path_of(&id(1)), Some("notes (conflict 01010101)"));
    }

    #[test]
    fn folded_layouts_are_the_same_in_any_order() {
        let a = [
            file(9, "A/Photo.jpg"),
            file(3, "a/photo.jpg"),
            file(4, "a/PHOTO.jpg"),
        ];
        let mut b = a.clone();
        b.reverse();
        let (a, b) = (
            Layout::folded(&a, Folding::Case),
            Layout::folded(&b, Folding::Case),
        );
        for n in [9, 3, 4] {
            assert_eq!(a.path_of(&id(n)), b.path_of(&id(n)));
        }
        assert_eq!(a.path_of(&id(3)), Some("a/photo.jpg"));
        assert_eq!(a.path_of(&id(4)), Some("a/PHOTO (conflict 04040404).jpg"));
        assert_eq!(a.path_of(&id(9)), Some("a/Photo (conflict 09090909).jpg"));
    }

    #[test]
    fn same_result_in_any_order() {
        let a = [
            file(9, "d/x.tar.gz"),
            file(3, "d/x.tar.gz"),
            file(4, "d/x.tar.gz"),
        ];
        let b = [
            file(4, "d/x.tar.gz"),
            file(9, "d/x.tar.gz"),
            file(3, "d/x.tar.gz"),
        ];
        let (la, lb) = (Layout::new(&a), Layout::new(&b));
        for n in [3, 4, 9] {
            assert_eq!(la.path_of(&id(n)), lb.path_of(&id(n)));
        }
        assert_eq!(la.path_of(&id(4)), Some("d/x.tar (conflict 04040404).gz"));
    }

    #[test]
    fn files_give_way_to_directories() {
        // A file at `a` clashes with `a/b`, which implies a directory `a`.
        let versions = [
            file(1, "a"),
            file(2, "a/b"),
            version(3, "c", EntryKind::Directory),
            file(4, "c"),
        ];
        let layout = Layout::new(&versions);
        assert_eq!(layout.path_of(&id(1)), Some("a (conflict 01010101)"));
        assert_eq!(layout.path_of(&id(2)), Some("a/b"));
        assert_eq!(layout.path_of(&id(3)), Some("c"));
        assert_eq!(layout.path_of(&id(4)), Some("c (conflict 04040404)"));
    }

    #[test]
    fn two_directories_are_one() {
        let versions = [
            version(7, "d", EntryKind::Directory),
            version(2, "d", EntryKind::Directory),
        ];
        let layout = Layout::new(&versions);
        assert_eq!(layout.path_of(&id(7)), Some("d"));
        assert_eq!(layout.path_of(&id(2)), Some("d"));
        assert_eq!(layout.entry_at("d"), Some(id(2)));
    }

    #[test]
    fn taken_conflict_name_uses_the_full_id() {
        let versions = [file(1, "f"), file(9, "f"), file(5, "f (conflict 09090909)")];
        let layout = Layout::new(&versions);
        assert_eq!(
            layout.path_of(&id(9)),
            Some("f (conflict 09090909090909090909090909090909)")
        );
        assert_eq!(layout.path_of(&id(5)), Some("f (conflict 09090909)"));
    }

    #[test]
    fn tombstones_are_ignored() {
        let versions = [file(1, "f"), version(0, "f", EntryKind::Tombstone)];
        let layout = Layout::new(&versions);
        assert_eq!(layout.path_of(&id(1)), Some("f"));
        assert_eq!(layout.path_of(&id(0)), None);
    }

    #[test]
    fn dotfiles_keep_their_name_as_stem() {
        assert_eq!(
            conflict_name(".bashrc", id(1), 8),
            ".bashrc (conflict 01010101)"
        );
        assert_eq!(
            conflict_name("a/Makefile", id(1), 8),
            "a/Makefile (conflict 01010101)"
        );
    }
}
