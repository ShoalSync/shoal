//! Detecting local changes by scanning a folder on disk.

use std::collections::BTreeSet;
use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use tidefell_chunker::{chunk_reader, Params};
use tidefell_proto::sync::{is_valid_path, EntryKind};
use tidefell_proto::{EntryId, Manifest, VersionVector};
use tracing::{debug, warn};

use crate::config::LocalDeletes;
use crate::engine::{FolderHandle, Inner};
use crate::ignores::{Ignores, IGNORE_FILE};
use crate::state::{FolderState, LocalFile};
use crate::{io_err, now_ms, random_bytes, Result};

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
}

impl Walk<'_> {
    fn dir(&mut self, dir: &Path, recursive: bool) {
        self.dirs.push(dir.to_owned());
        walk(
            self.root,
            dir,
            self.ignores,
            recursive,
            &mut self.files,
            &mut self.dirs,
        );
    }
}

fn walk(
    root: &Path,
    dir: &Path,
    ignores: &Ignores,
    recursive: bool,
    out: &mut Vec<(String, DiskStat)>,
    dirs: &mut Vec<PathBuf>,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        warn!(dir = %dir.display(), "cannot read directory");
        return;
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
        let rel = path
            .strip_prefix(root)
            .expect("walked paths are under root");
        let Some(rel) = rel
            .to_str()
            .map(|s| s.replace(std::path::MAIN_SEPARATOR, "/"))
        else {
            warn!(path = %path.display(), "skipping non-UTF-8 path");
            continue;
        };
        if ignores.is_ignored(&rel, file_type.is_dir()) {
            continue;
        }
        if file_type.is_dir() {
            if recursive {
                dirs.push(path.clone());
                walk(root, &path, ignores, true, out, dirs);
            }
        } else {
            if !is_valid_path(&rel) {
                warn!(path = %rel, "skipping path that cannot be synced");
                continue;
            }
            if let Some(stat) = stat(&path) {
                out.push((rel, stat));
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
        ..
    } = walker;
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
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
            Err(e) => {
                debug!(path = %rel, error = %e, "file vanished while scanning");
                continue;
            }
        };
        // The file itself is where its content is kept and read from (ADR 0027); only the
        // manifest is recorded.
        let manifest = chunk_reader(BufReader::new(file), Params::DEFAULT, |_, _| Ok(()))
            .map_err(io_err(&path))?;
        // A file still being written will be picked up by a later scan.
        if stat(&path) != Some(disk) {
            debug!(path = %rel, "file changed while scanning");
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
        let entry = match state.local.get(&c.rel).map(|l| l.entry) {
            Some(entry) => entry,
            // A file this device never had on disk here (a placeholder, a file dropped from
            // this device, or any file of a send-only folder) is the same entry only if it has
            // the same content. Otherwise it is a new file that happens to share the path, and
            // both are kept (ADR 0024) instead of it replacing the other device's file.
            None => match state.meta.find_path(&c.rel) {
                Some(v) if file_content(&v.kind) == Some(content) => v.entry,
                _ => EntryId(random_bytes()?),
            },
        };
        let up_to_date = state.meta.current(&entry).is_some_and(|v| {
            state.meta.disk_path(&entry) == Some(c.rel.as_str())
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
                &c.rel,
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
    let deleted: Vec<String> = known
        .keys()
        .filter(|rel| {
            !seen.contains(*rel)
                && scope.is_none_or(|s| s.contains(rel))
                && !state.ignores.is_ignored(rel, false)
                && fs::symlink_metadata(root.join(rel)).is_err()
        })
        .cloned()
        .collect();
    let deletes_here_only = state.config.sync.local_deletes == LocalDeletes::ThisDevice;
    for rel in deleted {
        let Some(local) = state.local.remove(&rel) else {
            continue;
        };
        state.used.remove(&rel);
        let live_here = state.meta.current(&local.entry).is_some_and(|v| {
            !matches!(v.kind, EntryKind::Tombstone)
                && state.meta.disk_path(&local.entry) == Some(rel.as_str())
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
                &rel,
                EntryKind::Tombstone,
                now_ms(),
            )?;
            index_changed = true;
        }
    }

    state.save()?;
    Ok(Scanned {
        changed: index_changed,
        dirs,
        conflicts,
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
