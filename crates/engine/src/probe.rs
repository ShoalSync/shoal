//! What a folder's filesystem does with names, found by trying them (ADR 0033).
//!
//! Error codes do not say why a name was refused: Android's shared storage reports a name FAT
//! refuses as "permission denied". So each name is created in a directory of its own under
//! `.shoal/`, and counts as stored only if the directory then lists exactly that name.

use std::fs;
use std::path::Path;

use crate::scan::RESERVED_DIR;

/// Characters some filesystems refuse in names: Windows' and FAT's, and a control character.
const CHARACTERS: [char; 8] = ['<', '>', ':', '"', '|', '?', '*', '\u{1}'];

/// Names Windows reserves, with or without an extension.
const RESERVED: [&str; 6] = ["CON", "PRN", "AUX", "NUL", "COM1", "LPT1"];

/// The longest name tried, in bytes.
const LONGEST: usize = 1024;

/// How a folder's filesystem treats names, probed when the folder opens (ADR 0033).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filesystem {
    /// Names that differ only in case are one file.
    pub case_insensitive: bool,
    /// Names that differ only in Unicode normalisation form are one file.
    pub unicode_insensitive: bool,
    /// Characters no name may contain.
    pub refused_characters: Vec<char>,
    /// Names refused whatever their case, such as Windows' `CON`.
    pub refused_names: Vec<String>,
    /// Those names are refused with an extension too, as in `CON.txt`.
    pub refused_with_extension: bool,
    /// Names may not end in a dot or a space.
    pub no_trailing_dot_or_space: bool,
    /// The longest name allowed, if known.
    pub max_name: Option<NameLength>,
}

/// The longest name a filesystem allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameLength {
    /// In bytes of UTF-8.
    Bytes(usize),
    /// In characters, as FAT and NTFS count them.
    Characters(usize),
}

impl Default for Filesystem {
    /// A filesystem that keeps every name as given, as Linux's do; what is assumed until a
    /// probe says otherwise.
    fn default() -> Self {
        Self {
            case_insensitive: false,
            unicode_insensitive: false,
            refused_characters: Vec::new(),
            refused_names: Vec::new(),
            refused_with_extension: false,
            no_trailing_dot_or_space: false,
            max_name: None,
        }
    }
}

impl Filesystem {
    /// Why a file at `path` cannot be stored here, if it cannot: the first name in it this
    /// filesystem refuses.
    pub fn refuses(&self, path: &str) -> Option<String> {
        path.split('/').find_map(|name| self.refuses_name(name))
    }

    fn refuses_name(&self, name: &str) -> Option<String> {
        if let Some(c) = name.chars().find(|c| self.refused_characters.contains(c)) {
            return Some(match c {
                c if c.is_control() => format!(
                    "{name:?} has a control character, which this folder's drive cannot store"
                ),
                c => format!("{name:?} has \"{c}\", which this folder's drive cannot store"),
            });
        }
        let upper = name.to_uppercase();
        let stem = upper.split('.').next().unwrap_or(&upper);
        if self
            .refused_names
            .iter()
            .any(|n| *n == upper || (self.refused_with_extension && n == stem))
        {
            return Some(format!("{name:?} is a name this folder's drive reserves"));
        }
        if self.no_trailing_dot_or_space && name.ends_with(['.', ' ']) {
            return Some(format!(
                "{name:?} ends in a dot or a space, which this folder's drive cannot store"
            ));
        }
        let too_long = match self.max_name {
            Some(NameLength::Bytes(n)) => name.len() > n,
            Some(NameLength::Characters(n)) => name.encode_utf16().count() > n,
            None => false,
        };
        too_long.then(|| format!("{name:?} is longer than this folder's drive allows"))
    }

    /// What the probe found, in a sentence, for `shoal doctor`.
    pub fn describe(&self) -> String {
        let mut parts = vec![if self.case_insensitive {
            "names match without regard to case".to_owned()
        } else {
            "names differing in case are different files".to_owned()
        }];
        if self.unicode_insensitive {
            parts.push("names match across Unicode forms".to_owned());
        }
        if !self.refused_characters.is_empty() {
            let printable: String = self
                .refused_characters
                .iter()
                .filter(|c| !c.is_control())
                .flat_map(|c| [*c, ' '])
                .collect();
            parts.push(format!("refuses {}in names", printable));
        }
        if !self.refused_names.is_empty() {
            parts.push(format!("reserves {}", self.refused_names.join(", ")));
        }
        if self.no_trailing_dot_or_space {
            parts.push("refuses names ending in a dot or a space".to_owned());
        }
        match self.max_name {
            Some(NameLength::Bytes(n)) => parts.push(format!("names up to {n} bytes")),
            Some(NameLength::Characters(n)) => parts.push(format!("names up to {n} characters")),
            None => {}
        }
        let mut text = parts.join("; ");
        if let Some(first) = text.get(..1) {
            text = first.to_uppercase() + text.get(1..).unwrap_or_default();
        }
        text
    }
}

/// Probes the filesystem of the folder at `root`, in a directory of its own under
/// `.shoal/`. Fails if that directory cannot be made, as on a read-only drive.
pub(crate) fn probe(root: &Path) -> std::io::Result<Filesystem> {
    let dir = root
        .join(RESERVED_DIR)
        .join(format!("probe-{:016x}", rand_suffix()));
    fs::create_dir_all(&dir)?;
    let found = run(&dir);
    let _ = fs::remove_dir_all(&dir);
    Ok(found)
}

fn rand_suffix() -> u64 {
    crate::random_bytes().map_or(0, u64::from_le_bytes)
}

fn run(dir: &Path) -> Filesystem {
    let stored = |name: &str| stored(dir, name);
    let case_insensitive = matches_other(dir, "Aa", "aA");
    let unicode_insensitive = matches_other(dir, "\u{e9}", "e\u{301}");
    let refused_characters = CHARACTERS
        .iter()
        .copied()
        .filter(|c| !stored(&format!("a{c}b")))
        .collect();
    let refused_names: Vec<String> = RESERVED
        .iter()
        .filter(|n| !stored(n))
        .map(|n| (*n).to_owned())
        .collect();
    let refused_with_extension = refused_names
        .first()
        .is_some_and(|n| !stored(&format!("{n}.txt")));
    let no_trailing_dot_or_space = !stored("a.") && !stored("a ");
    Filesystem {
        case_insensitive,
        unicode_insensitive,
        refused_characters,
        refused_names,
        refused_with_extension,
        no_trailing_dot_or_space,
        max_name: max_name(dir),
    }
}

/// True if a file created as `name` is listed under exactly that name. Whatever it made is
/// removed.
fn stored(dir: &Path, name: &str) -> bool {
    if fs::File::create(dir.join(name)).is_err() {
        return false;
    }
    let listed: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    for other in &listed {
        let _ = fs::remove_file(dir.join(other));
    }
    listed.iter().any(|n| n == name)
}

/// True if, once `name` is created, `other` names the same file.
fn matches_other(dir: &Path, name: &str, other: &str) -> bool {
    if fs::File::create(dir.join(name)).is_err() {
        return false;
    }
    let same = fs::symlink_metadata(dir.join(other)).is_ok();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let _ = fs::remove_file(e.path());
        }
    }
    same
}

/// The longest name the filesystem stores: the longest ASCII name, then whether as many
/// two-byte characters fit too, which means the limit counts characters.
fn max_name(dir: &Path) -> Option<NameLength> {
    let fits = |len: usize| stored(dir, &"a".repeat(len));
    if !fits(1) {
        return None;
    }
    let (mut ok, mut too_long) = (1, LONGEST + 1);
    if fits(LONGEST) {
        return Some(NameLength::Bytes(LONGEST));
    }
    while too_long - ok > 1 {
        let mid = ok + (too_long - ok) / 2;
        if fits(mid) {
            ok = mid;
        } else {
            too_long = mid;
        }
    }
    Some(if stored(dir, &"\u{e9}".repeat(ok)) {
        NameLength::Characters(ok)
    } else {
        NameLength::Bytes(ok)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Linux's filesystems keep every name; NTFS ignores case by default.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_filesystem_keeps_every_name() {
        let dir = tempfile::tempdir().unwrap();
        let found = probe(dir.path()).unwrap();
        // The test machine's filesystem: Linux keeps names as given.
        assert!(!found.case_insensitive);
        assert!(!found.unicode_insensitive || cfg!(target_os = "macos"));
        assert!(found.refused_characters.is_empty(), "{found:?}");
        assert!(found.refused_names.is_empty());
        assert!(!found.no_trailing_dot_or_space);
        assert!(matches!(found.max_name, Some(NameLength::Bytes(n)) if n >= 255));
        assert_eq!(
            fs::read_dir(dir.path().join(RESERVED_DIR)).unwrap().count(),
            0,
            "the probe cleans up"
        );
    }

    #[test]
    fn fat_names() {
        let fat = Filesystem {
            case_insensitive: true,
            refused_characters: CHARACTERS.to_vec(),
            no_trailing_dot_or_space: true,
            max_name: Some(NameLength::Characters(255)),
            ..Filesystem::default()
        };
        assert!(fat.refuses("a:b.txt").is_some());
        assert!(fat.refuses("dir?/x.txt").is_some());
        assert!(fat.refuses("dir/x.").is_some());
        assert!(fat.refuses("ok/fine.txt").is_none());
        assert!(fat.refuses(&"\u{e9}".repeat(255)).is_none());
        assert!(fat.refuses(&"\u{e9}".repeat(256)).is_some());
        let windows = Filesystem {
            refused_names: vec!["CON".to_owned()],
            refused_with_extension: true,
            ..fat
        };
        assert!(windows.refuses("con").is_some());
        assert!(windows.refuses("Con.txt").is_some());
        assert!(windows.refuses("console.txt").is_none());
        assert!(Filesystem::default().refuses("a:b?.").is_none());
    }

    #[test]
    fn descriptions() {
        assert_eq!(
            Filesystem::default().describe(),
            "Names differing in case are different files"
        );
        let fat = Filesystem {
            case_insensitive: true,
            refused_characters: vec![':', '?', '\u{1}'],
            no_trailing_dot_or_space: true,
            max_name: Some(NameLength::Characters(255)),
            ..Filesystem::default()
        };
        assert_eq!(
            fat.describe(),
            "Names match without regard to case; refuses : ? in names; \
             refuses names ending in a dot or a space; names up to 255 characters"
        );
    }
}
