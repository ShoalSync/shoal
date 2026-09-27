//! Ignore patterns (ADR 0029): paths this device neither sends nor writes.

use std::fs;
use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tidefell_proto::sync::is_valid_path;
use tracing::warn;

/// The file at a folder's root that lists this device's patterns. It is never synced.
pub(crate) const IGNORE_FILE: &str = ".tidefellignore";

/// Operating-system and temporary files, matched before the folder's own patterns so `!` can
/// include one of them again.
const DEFAULTS: &[&str] = &[
    ".DS_Store",
    "._*",
    ".Spotlight-V100/",
    ".Trashes/",
    ".fseventsd/",
    "Thumbs.db",
    "ehthumbs.db",
    "desktop.ini",
    "$RECYCLE.BIN/",
    "~$*",
    ".~lock.*#",
    "*.part",
    "*.crdownload",
    "*.swp",
    "*.swo",
];

/// A folder's ignore patterns on this device.
#[derive(Debug, Clone)]
pub(crate) struct Ignores {
    patterns: Gitignore,
    /// The folder-relative files the ignore file includes.
    includes: Vec<String>,
}

impl Ignores {
    /// The built-in defaults and the patterns in `root`'s ignore file, with its `#include`
    /// lines read from files inside the folder.
    pub(crate) fn load(root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        for line in DEFAULTS {
            add(&mut builder, line);
        }
        let mut includes = Vec::new();
        if let Ok(text) = fs::read_to_string(root.join(IGNORE_FILE)) {
            for line in text.lines() {
                let Some(include) = line.strip_prefix("#include ") else {
                    add(&mut builder, line);
                    continue;
                };
                let include = include.trim();
                includes.push(include.to_owned());
                match is_valid_path(include).then(|| fs::read_to_string(root.join(include))) {
                    Some(Ok(text)) => text.lines().for_each(|line| add(&mut builder, line)),
                    _ => {
                        warn!(folder = %root.display(), path = include, "cannot include ignore patterns")
                    }
                }
            }
        }
        match builder.build() {
            Ok(patterns) => Self { patterns, includes },
            Err(e) => {
                warn!(folder = %root.display(), error = %e, "ignore patterns not applied");
                Self::defaults(root)
            }
        }
    }

    /// The built-in defaults only.
    pub(crate) fn defaults(root: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(root);
        for line in DEFAULTS {
            add(&mut builder, line);
        }
        Self {
            patterns: builder.build().expect("the defaults are valid patterns"),
            includes: Vec::new(),
        }
    }

    /// Whether the folder-relative `path`, or a directory it is in, is ignored.
    pub(crate) fn is_ignored(&self, path: &str, is_dir: bool) -> bool {
        path == IGNORE_FILE
            || self
                .patterns
                .matched_path_or_any_parents(path, is_dir)
                .is_ignore()
    }
}

impl Ignores {
    /// Whether `path` is the ignore file or a file it includes.
    pub(crate) fn is_pattern_file(&self, path: &str) -> bool {
        path == IGNORE_FILE || self.includes.iter().any(|i| i == path)
    }
}

fn add(builder: &mut GitignoreBuilder, line: &str) {
    if let Err(e) = builder.add_line(None, line) {
        warn!(pattern = line, error = %e, "skipping ignore pattern");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignores(file: &str, shared: Option<&str>) -> (tempfile::TempDir, Ignores) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(IGNORE_FILE), file).unwrap();
        if let Some(shared) = shared {
            fs::write(dir.path().join("shared-ignore"), shared).unwrap();
        }
        let ignores = Ignores::load(dir.path());
        (dir, ignores)
    }

    #[test]
    fn defaults_and_patterns() {
        let (_dir, i) = ignores("build/\n*.log\n!keep.log\n/top.txt\n# a comment\n", None);
        assert!(i.is_ignored(".DS_Store", false));
        assert!(i.is_ignored("photos/.DS_Store", false));
        assert!(i.is_ignored("report.docx.part", false));
        assert!(i.is_ignored("build", true));
        assert!(i.is_ignored("build/out/app.bin", false));
        assert!(i.is_ignored("src/debug.log", false));
        assert!(!i.is_ignored("keep.log", false));
        assert!(i.is_ignored("top.txt", false));
        assert!(!i.is_ignored("nested/top.txt", false));
        assert!(!i.is_ignored("photos/beach.jpg", false));
    }

    #[test]
    fn the_ignore_file_is_never_synced() {
        let (_dir, i) = ignores("!.tidefellignore\n", None);
        assert!(i.is_ignored(IGNORE_FILE, false));
        assert!(!i.is_ignored("sub/.tidefellignore", false));
    }

    #[test]
    fn defaults_can_be_included_again() {
        let (_dir, i) = ignores("!*.part\n", None);
        assert!(!i.is_ignored("video.part", false));
        assert!(i.is_ignored(".DS_Store", false));
    }

    #[test]
    fn include_reads_a_synced_file() {
        let (_dir, i) = ignores("#include shared-ignore\n*.tmp\n", Some("node_modules/\n"));
        assert!(i.is_ignored("app/node_modules/x/index.js", false));
        assert!(i.is_ignored("a.tmp", false));
        let (_dir, i) = ignores("#include ../outside\n", None);
        assert!(!i.is_ignored("anything", false));
    }
}
