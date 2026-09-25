//! Where each live entry is written on disk when several claim the same path (ADR 0024,
//! "Entries at the same path" in `specs/sync-v1.md`).
//!
//! Every replica computes the same layout from the same entries, so no versions are made to
//! resolve a clash.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::EntryId;

/// The on-disk path of every live entry.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    paths: HashMap<EntryId, String>,
    entries: HashMap<String, EntryId>,
}

impl Layout {
    /// Lays out the current versions of live entries. Tombstones are ignored.
    pub fn new<'a>(versions: impl IntoIterator<Item = &'a EntryVersion>) -> Self {
        let live: Vec<&EntryVersion> = versions
            .into_iter()
            .filter(|v| !matches!(v.kind, EntryKind::Tombstone))
            .collect();

        // Paths that are directories: explicit ones and the ancestors of every entry.
        let mut dirs: BTreeSet<&str> = BTreeSet::new();
        for v in &live {
            if matches!(v.kind, EntryKind::Directory) {
                dirs.insert(&v.path);
            }
            let mut rest = v.path.as_str();
            while let Some((parent, _)) = rest.rsplit_once('/') {
                dirs.insert(parent);
                rest = parent;
            }
        }

        // Files and symlinks, grouped by path, lowest entry ID first.
        let mut claims: BTreeMap<&str, Vec<EntryId>> = BTreeMap::new();
        let mut layout = Self::default();
        for v in &live {
            if matches!(v.kind, EntryKind::Directory) {
                layout.paths.insert(v.entry, v.path.clone());
                let lowest = layout.entries.entry(v.path.clone()).or_insert(v.entry);
                *lowest = (*lowest).min(v.entry);
            } else {
                claims.entry(&v.path).or_default().push(v.entry);
            }
        }

        let mut taken: BTreeSet<String> = dirs.iter().map(|d| (*d).to_owned()).collect();
        let mut displaced: Vec<(EntryId, &str)> = Vec::new();
        for (path, mut ids) in claims {
            ids.sort();
            let keeps = if dirs.contains(path) { 0 } else { 1 };
            for (i, id) in ids.into_iter().enumerate() {
                if i < keeps {
                    layout.place(id, path.to_owned());
                    taken.insert(path.to_owned());
                } else {
                    displaced.push((id, path));
                }
            }
        }
        displaced.sort();
        for (id, path) in displaced {
            let mut name = conflict_name(path, id, 8);
            if taken.contains(&name) {
                name = conflict_name(path, id, 32);
            }
            taken.insert(name.clone());
            layout.place(id, name);
        }
        layout
    }

    fn place(&mut self, entry: EntryId, path: String) {
        self.entries.insert(path.clone(), entry);
        self.paths.insert(entry, path);
    }

    /// Where `entry` is on disk, if it is live.
    pub fn path_of(&self, entry: &EntryId) -> Option<&str> {
        self.paths.get(entry).map(String::as_str)
    }

    /// The live entry on disk at `path`. For a directory claimed by several entries, the one
    /// with the lowest ID.
    pub fn entry_at(&self, path: &str) -> Option<EntryId> {
        self.entries.get(path).copied()
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
