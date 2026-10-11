//! Detecting local changes by scanning a folder on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use tidefell_chunker::{chunk_reader, Params};
use tidefell_proto::sync::{is_valid_path, EntryKind};
use tidefell_proto::{EntryId, Manifest, VersionVector};
use tracing::{debug, warn};

use crate::activity::Failure;
use crate::config::LocalDeletes;
use crate::engine::{FolderHandle, Inner};
use crate::ignores::{Ignores, IGNORE_FILE};
use crate::state::{recorded, FolderState, LocalFile};
use crate::views::{Operation, ProblemKind};
use crate::{now_ms, random_bytes, Result};

/// Directory at a folder's root reserved for the engine's own files.
pub(crate) const RESERVED_DIR: &str = ".tidefell";

/// What a file on disk looks like, cheaply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiskStat {
    pub(crate) size: u64,
    pub(crate) mtime_ns: i128,
    pub(crate) executable: bool,
}

pub(crate) fn stat(path: &Path) -> Option<DiskStat> {
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = false;
    Some(DiskStat {
        size: meta.len(),
        mtime_ns,
        executable,
    })
}

/// True if the file at `path` is exactly what the engine last saw or wrote there.
/// The vector a local change at `rel` builds on: the recorded base of the content on disk, or,
/// for a file recorded before bases were kept or not recorded at all, everything known of
/// `entry`.
pub(crate) fn base_of(state: &FolderState, rel: &str, entry: EntryId) -> VersionVector {
    match state.local.get(rel) {
        Some(local) if local.entry == entry && !local.base.is_empty() => local.base.clone(),
        _ => state.meta.current_vector(&entry),
    }
}

pub(crate) fn unchanged(path: &Path, local: &LocalFile) -> bool {
    stat(path).is_some_and(|s| s.size == local.size && s.mtime_ns == local.mtime_ns)
}

/// The part of a folder a scan looks at: folder-relative directories (`""` is the root), each
/// with or without everything below it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Scope(BTreeSet<(String, bool)>);

impl Scope {
    /// The directory `dir` itself, and everything below it if `recursive`.
    pub(crate) fn add(&mut self, dir: &str, recursive: bool) {
        self.0.insert((dir.to_owned(), recursive));
    }

    /// Whether the file at `path` is in the scope.
    pub(crate) fn contains(&self, path: &str) -> bool {
        let parent = path.rsplit_once('/').map_or("", |(dir, _)| dir);
        self.0.iter().any(|(dir, recursive)| {
            parent == dir
                || (*recursive
                    && (path == dir || dir.is_empty() || path.starts_with(&format!("{dir}/"))))
        })
    }
}

/// Files and directories under `dir`, all the way down if `recursive`, skipping the reserved
/// directory, ignored paths, symlinks and special files.
struct Walk<'a> {
    root: &'a Path,
    ignores: &'a Ignores,
    files: Vec<(String, DiskStat)>,
    dirs: Vec<PathBuf>,
    /// Paths that could not be read or synced (ADR 0030).
    failures: Vec<Failure>,
    /// Directories that could not be read: what is under them is not known to be gone.
    unreadable: Vec<String>,
}

impl Walk<'_> {
    fn dir(&mut self, dir: &Path, recursive: bool) {
        self.dirs.push(dir.to_owned());
        let (root, ignores) = (self.root, self.ignores);
        walk(root, dir, ignores, recursive, self);
    }
}

/// How a directory's problems are keyed: its path with a `/` after it, so it is told apart
/// from a file's.
fn dir_key(rel: &str) -> String {
    format!("{rel}/")
}

/// True if `error` says the path is not there: only then is a recorded file deleted.
fn is_gone(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// `path` relative to `root`, `/`-separated, if it is valid UTF-8.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path
        .strip_prefix(root)
        .expect("walked paths are under root");
    rel.to_str()
        .map(|s| s.replace(std::path::MAIN_SEPARATOR, "/"))
}

fn walk(root: &Path, dir: &Path, ignores: &Ignores, recursive: bool, found: &mut Walk<'_>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "cannot read directory");
            let rel = relative(root, dir).unwrap_or_else(|| dir.to_string_lossy().into_owned());
            found
                .failures
                .push(Failure::io(&dir_key(&rel), Operation::Scan, &e));
            found.unreadable.push(rel);
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if dir == root && entry.file_name() == RESERVED_DIR {
            continue;
        }
        if !file_type.is_dir() && !file_type.is_file() {
            // Symlinks and special files are not synced yet.
            continue;
        }
        let Some(rel) = relative(root, &path) else {
            warn!(path = %path.display(), "skipping non-UTF-8 path");
            let lossy = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            found.failures.push(Failure::new(
                &lossy,
                Operation::Scan,
                ProblemKind::NameNotAllowed,
                "the name is not valid UTF-8",
            ));
            continue;
        };
        if ignores.is_ignored(&rel, file_type.is_dir()) {
            continue;
        }
        if file_type.is_dir() {
            if recursive {
                found.dirs.push(path.clone());
                walk(root, &path, ignores, true, found);
            }
        } else {
            if !is_valid_path(&recorded(&rel)) {
                warn!(path = %rel, "skipping path that cannot be synced");
                found.failures.push(Failure::new(
                    &rel,
                    Operation::Scan,
                    ProblemKind::NameNotAllowed,
                    "the name cannot be synced",
                ));
                continue;
            }
            match stat(&path) {
                Some(stat) => found.files.push((rel, stat)),
                None => {
                    if let Err(e) = fs::symlink_metadata(&path) {
                        if !is_gone(&e) {
                            warn!(path = %rel, error = %e, "cannot look at file");
                            found.failures.push(Failure::io(&rel, Operation::Scan, &e));
                        }
                    }
                }
            }
        }
    }
}

fn file_content(kind: &EntryKind) -> Option<tidefell_proto::ContentHash> {
    match kind {
        EntryKind::File { content, .. } => Some(*content),
        _ => None,
    }
}

/// Keeps one file for each path the files found are recorded as. Two names on disk can be one
/// path as recorded, such as a name in two Unicode forms on a filesystem that keeps both: the
/// one this device already has keeps it, or else the first, and the others are problems
/// (ADR 0033).
fn one_per_path(
    found: Vec<(String, DiskStat)>,
    known: &BTreeMap<String, LocalFile>,
    failures: &mut Vec<Failure>,
) -> Vec<(String, DiskStat)> {
    let mut by_path: BTreeMap<String, Vec<(String, DiskStat)>> = BTreeMap::new();
    for (rel, stat) in found {
        by_path
            .entry(recorded(&rel).into_owned())
            .or_default()
            .push((rel, stat));
    }
    let mut kept = Vec::new();
    for (path, mut names) in by_path {
        if names.len() > 1 {
            let keep = names
                .iter()
                .position(|(rel, _)| known.contains_key(rel))
                .unwrap_or(0);
            let first = names.swap_remove(keep);
            for (rel, _) in names {
                warn!(path = %rel, recorded = %path, "skipping a second name for one path");
                failures.push(Failure::new(
                    &rel,
                    Operation::Scan,
                    ProblemKind::NameNotAllowed,
                    format!("{} has the same name, written another way", first.0),
                ));
            }
            kept.push(first);
        } else {
            kept.extend(names);
        }
    }
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    kept
}

struct Changed {
    rel: String,
    manifest: Manifest,
    stat: DiskStat,
}

/// What a scan found.
#[derive(Debug, Default)]
pub(crate) struct Scanned {
    /// Whether the index changed.
    pub(crate) changed: bool,
    /// The directories walked, to watch for later changes.
    pub(crate) dirs: Vec<PathBuf>,
    /// Files whose changes conflict with a version that arrived before they were scanned.
    pub(crate) conflicts: Vec<String>,
    /// Whether the set of paths with problems changed (ADR 0030).
    pub(crate) problems_changed: bool,
}

/// Scans `id` for changes since the last scan and records them: everything, or only `scope`.
/// `saved` is a file written through the engine, recorded even if it is ignored.
pub(crate) fn scan(
    inner: &Inner,
    handle: &FolderHandle,
    scope: Option<&Scope>,
    saved: Option<&str>,
) -> Result<Scanned> {
    let (root, known) = {
        let state = handle.lock();
        (state.config.path.clone(), state.local.clone())
    };

    let ignores = Ignores::load(&root);
    let mut walker = Walk {
        root: &root,
        ignores: &ignores,
        files: Vec::new(),
        dirs: Vec::new(),
        failures: Vec::new(),
        unreadable: Vec::new(),
    };
    match scope {
        None => walker.dir(&root, true),
        Some(scope) => {
            for (dir, recursive) in &scope.0 {
                let abs = root.join(dir);
                let reserved = dir == RESERVED_DIR || dir.starts_with(&format!("{RESERVED_DIR}/"));
                if abs.is_dir() && !reserved && (dir.is_empty() || !ignores.is_ignored(dir, true)) {
                    walker.dir(&abs, *recursive);
                }
            }
        }
    }
    let Walk {
        files: mut found,
        dirs,
        mut failures,
        unreadable,
        ..
    } = walker;
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let found = one_per_path(found, &known, &mut failures);
    let mut found = found;
    if let Some(rel) = saved.filter(|rel| *rel != IGNORE_FILE && ignores.is_ignored(rel, false)) {
        if let Some(stat) = stat(&root.join(rel)) {
            found.push((rel.to_owned(), stat));
        }
    }

    let mut seen = BTreeSet::new();
    let mut changed = Vec::new();
    for (rel, disk) in found {
        seen.insert(rel.clone());
        if known.get(&rel).is_some_and(|l| {
            l.size == disk.size && l.mtime_ns == disk.mtime_ns && l.executable == disk.executable
        }) {
            continue;
        }
        let path = root.join(&rel);
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %rel, "file vanished while scanning");
                continue;
            }
            Err(e) => {
                warn!(path = %rel, error = %e, "cannot open file");
                failures.push(Failure::io(&rel, Operation::Scan, &e));
                continue;
            }
        };
        // The file itself is where its content is kept and read from (ADR 0027); only the
        // manifest is recorded. One file that cannot be read does not stop the scan.
        let manifest = match chunk_reader(BufReader::new(file), Params::DEFAULT, |_, _| Ok(())) {
            Ok(manifest) => manifest,
            Err(e) => {
                warn!(path = %rel, error = %e, "cannot read file");
                failures.push(Failure::io(&rel, Operation::Scan, &e));
                continue;
            }
        };
        // A file still being written will be picked up by a later scan.
        if stat(&path) != Some(disk) {
            debug!(path = %rel, "file changed while scanning");
            failures.push(Failure::new(
                &rel,
                Operation::Scan,
                ProblemKind::ChangedWhileWriting,
                "the file changed while it was read; it is read again at the next scan",
            ));
            continue;
        }
        changed.push(Changed {
            rel,
            manifest,
            stat: disk,
        });
    }

    let mut state = handle.lock();
    state.ignores = ignores;
    let mut index_changed = false;
    let mut conflicts = Vec::new();
    let now = now_ms();
    for c in changed {
        let content = c.manifest.content;
        // Recorded in NFC, whatever form the name has on disk (ADR 0033).
        let path = recorded(&c.rel).into_owned();
        let entry = match state.local.get(&c.rel).map(|l| l.entry) {
            Some(entry) => entry,
            // A file this device never had on disk here (a placeholder, a file dropped from
            // this device, or any file of a send-only folder) is the same entry only if it has
            // the same content. Otherwise it is a new file that happens to share the path, and
            // both are kept (ADR 0024) instead of it replacing the other device's file.
            None => match state.meta.find_path(&path) {
                Some(v) if file_content(&v.kind) == Some(content) => v.entry,
                _ => EntryId(random_bytes()?),
            },
        };
        let up_to_date = state.meta.current(&entry).is_some_and(|v| {
            state.meta.disk_path(&entry) == Some(path.as_str())
                && matches!(v.kind, EntryKind::File { content: current, executable, .. }
                    if current == content && executable == c.stat.executable)
        });
        state.add_manifest(c.manifest);
        let base = if up_to_date {
            state.meta.current_vector(&entry)
        } else {
            // Build on the version the file had on disk, not on versions that arrived since and
            // were never written, so those become conflict copies instead of being lost.
            let base = base_of(&state, &c.rel, entry);
            let open = state.meta.open_conflicts(&entry).count();
            let (_, vector) = state.meta.local_change_from(
                inner.key.signing_key(),
                entry,
                &base,
                &path,
                EntryKind::File {
                    content,
                    size: c.stat.size,
                    executable: c.stat.executable,
                },
                (c.stat.mtime_ns / 1_000_000) as i64,
            )?;
            if state.meta.open_conflicts(&entry).count() > open {
                conflicts.push(c.rel.clone());
            }
            index_changed = true;
            vector
        };
        state.used.insert(c.rel.clone(), now);
        // A file put back by hand is on this device again.
        state.dropped.remove(&entry);
        state.local.insert(
            c.rel,
            LocalFile {
                entry,
                content,
                size: c.stat.size,
                mtime_ns: c.stat.mtime_ns,
                executable: c.stat.executable,
                base,
            },
        );
    }

    // The drive may have gone while the folder was walked, and an unmounted root looks like
    // every file deleted. Check it again right before recording any delete (ADR 0037).
    if let Some(reason) = crate::root::check(&root, state.config.id) {
        state.save()?;
        return Err(crate::Error::FolderUnavailable(state.config.id, reason));
    }

    // A path that became ignored after it synced keeps its entry: nothing is deleted. Only
    // files in the scope that are verifiably gone are.
    // Only a path known to be gone is deleted. One that cannot be looked at, such as in a
    // directory this device may list but not enter, is a problem, not a delete (ADR 0030).
    let mut deleted = Vec::new();
    for rel in known.keys() {
        if seen.contains(rel)
            || !scope.is_none_or(|s| s.contains(rel))
            || state.ignores.is_ignored(rel, false)
            || unreadable
                .iter()
                .any(|dir| dir.is_empty() || rel.starts_with(&format!("{dir}/")))
        {
            continue;
        }
        match fs::symlink_metadata(root.join(rel)) {
            Ok(_) => {}
            Err(e) if is_gone(&e) => deleted.push(rel.clone()),
            Err(e) => {
                if !failures.iter().any(|f| f.path == *rel) {
                    warn!(path = %rel, error = %e, "cannot look at file");
                    failures.push(Failure::io(rel, Operation::Scan, &e));
                }
            }
        }
    }
    let deletes_here_only = state.config.sync.local_deletes == LocalDeletes::ThisDevice;
    for rel in deleted {
        let Some(local) = state.local.remove(&rel) else {
            continue;
        };
        state.used.remove(&rel);
        let live_here = state.meta.current(&local.entry).is_some_and(|v| {
            !matches!(v.kind, EntryKind::Tombstone)
                && state
                    .meta
                    .disk_path(&local.entry)
                    .is_some_and(|p| state.same_place(&rel, p))
        });
        if live_here && deletes_here_only {
            // ADR 0025: dropped from this device only; it stays on every other device.
            debug!(path = %rel, "dropped from this device");
            state.dropped.insert(local.entry);
        } else if live_here {
            let base = if !local.base.is_empty() {
                local.base.clone()
            } else {
                state.meta.current_vector(&local.entry)
            };
            state.meta.local_change_from(
                inner.key.signing_key(),
                local.entry,
                &base,
                &recorded(&rel),
                EntryKind::Tombstone,
                now_ms(),
            )?;
            index_changed = true;
        }
    }

    state.save()?;
    drop(state);
    // Problems the scan covered are replaced by what it found: a path read, gone or now
    // ignored has none.
    // A directory's problem is covered only if the scan tried to read that directory.
    let tried: BTreeSet<String> = dirs
        .iter()
        .filter_map(|d| relative(&root, d))
        .map(|rel| dir_key(&rel))
        .collect();
    let problems_changed = handle.activity.lock().unwrap().settle(
        &[Operation::Scan],
        |path| {
            if path.ends_with('/') {
                tried.contains(path)
            } else {
                scope.is_none_or(|s| s.contains(path))
            }
        },
        failures,
        now_ms(),
    );
    Ok(Scanned {
        changed: index_changed,
        dirs,
        conflicts,
        problems_changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_alone_covers_its_own_files() {
        let mut scope = Scope::default();
        scope.add("photos", false);
        assert!(scope.contains("photos/a.jpg"));
        assert!(!scope.contains("photos/2024/b.jpg"));
        assert!(!scope.contains("photos.txt"));
        assert!(!scope.contains("photosx/a.jpg"));
        assert!(!scope.contains("other/a.jpg"));
    }

    #[test]
    fn the_root_alone_covers_only_top_level_files() {
        let mut scope = Scope::default();
        scope.add("", false);
        assert!(scope.contains("a.txt"));
        assert!(!scope.contains("dir/a.txt"));
    }

    #[test]
    fn a_recursive_directory_covers_everything_below() {
        let mut scope = Scope::default();
        scope.add("photos", true);
        assert!(scope.contains("photos/a.jpg"));
        assert!(scope.contains("photos/2024/06/b.jpg"));
        assert!(!scope.contains("photosx/a.jpg"));
        assert!(!scope.contains("a.jpg"));
        let mut all = Scope::default();
        all.add("", true);
        assert!(all.contains("a/b/c.txt"));
    }

    #[test]
    fn nothing_outside_every_scope() {
        let mut scope = Scope::default();
        scope.add("a", false);
        scope.add("b/c", true);
        for path in ["x.txt", "a/b/x.txt", "b/x.txt", "b/cd/x.txt", "c/x.txt"] {
            assert!(!scope.contains(path), "{path}");
        }
        assert!(!Scope::default().contains("x.txt"));
    }
}
