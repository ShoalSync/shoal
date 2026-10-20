//! Detecting local changes by scanning a folder on disk.

use std::collections::{BTreeSet, HashMap, HashSet};
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
use crate::{random_bytes, Result};

/// Directory at a folder's root reserved for the engine's own files.
pub(crate) const RESERVED_DIR: &str = ".tidefell";

/// What a file on disk looks like, cheaply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiskStat {
    pub(crate) size: u64,
    pub(crate) mtime_ns: i128,
    /// Whether the file is executable, or None where the filesystem has no such bit, as on
    /// Windows: there it is unknown, matches whatever was recorded, and a change keeps the
    /// entry's bit (ADR 0042).
    pub(crate) executable: Option<bool>,
    /// The file's inode, or 0 where there is none: a file moved over another of the same size
    /// and modification time is still told apart from it (ADR 0036).
    pub(crate) inode: u64,
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
        Some(meta.permissions().mode() & 0o111 != 0)
    };
    #[cfg(not(unix))]
    let executable = None;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some(DiskStat {
        size: meta.len(),
        mtime_ns,
        executable,
        inode,
    })
}

/// True if the file at `path` is exactly what the engine last saw or wrote there.
/// The vector a local change at `rel` builds on: the recorded base of the content on disk, or,
/// for a file recorded before bases were kept or not recorded at all, everything known of
/// `entry`.
pub(crate) fn base_of(state: &FolderState, rel: &str, entry: EntryId) -> VersionVector {
    match state.local.get(rel) {
        Some(local) if local.entry == entry && !local.base_is_empty() => local.base(),
        _ => state.meta.current_vector(&entry),
    }
}

/// True if the file at `path` has `size`, `mtime_ns` and, unless 0 (not known), `inode`.
pub(crate) fn unchanged(path: &Path, size: u64, mtime_ns: i128, inode: u64) -> bool {
    stat(path).is_some_and(|s| {
        s.size == size && s.mtime_ns == mtime_ns && (inode == 0 || s.inode == inode)
    })
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

/// The path to record for a change to `entry`, whose file is at `path` here: that path, unless
/// the file is somewhere other than its recorded path only because this device folds names, in
/// which case it keeps its recorded path, so other devices are not renamed (ADR 0033).
fn recorded_path(state: &FolderState, entry: EntryId, path: &str) -> String {
    match state.meta.current(&entry) {
        Some(v)
            if state.meta.gives_way_here_only(&entry)
                && state.meta.disk_path(&entry) == Some(path) =>
        {
            v.path.to_owned()
        }
        _ => path.to_owned(),
    }
}

/// Keeps one file for each path the files found are recorded as. Two names on disk can be one
/// path as recorded, such as a name in two Unicode forms on a filesystem that keeps both: the
/// one this device already has keeps it, or else the first, and the others are problems
/// (ADR 0033).
fn one_per_path(
    mut found: Vec<(String, DiskStat)>,
    handle: &FolderHandle,
    folding: crate::state::NameFolding,
    failures: &mut Vec<Failure>,
) -> Vec<(String, DiskStat)> {
    // Found sorted and without repeats: on a drive that tells every name apart, names in NFC
    // cannot collide, so there is nothing to group (ADR 0035: no copy of every path).
    let normalized = |rel: &str| matches!(recorded(rel), std::borrow::Cow::Borrowed(_));
    if folding.0 == tidefell_meta::Folding::None && found.iter().all(|(rel, _)| normalized(rel)) {
        return found;
    }
    found.sort_by_cached_key(|(rel, _)| folding.fold(rel));
    let mut kept: Vec<(String, DiskStat)> = Vec::with_capacity(found.len());
    let mut group: Vec<(String, DiskStat)> = Vec::new();
    let mut flush = |group: &mut Vec<(String, DiskStat)>, kept: &mut Vec<(String, DiskStat)>| {
        if group.len() > 1 {
            let keep = {
                let state = handle.lock();
                group
                    .iter()
                    .position(|(rel, _)| state.local.get(rel).is_some())
                    .unwrap_or(0)
            };
            let first = group.swap_remove(keep);
            for (rel, _) in group.drain(..) {
                warn!(path = %rel, "skipping a second name for one path");
                failures.push(Failure::new(
                    &rel,
                    Operation::Scan,
                    ProblemKind::NameNotAllowed,
                    format!("{} has the same name, written another way", first.0),
                ));
            }
            kept.push(first);
        } else {
            kept.append(group);
        }
    };
    for item in found {
        if group
            .last()
            .is_some_and(|(rel, _)| folding.fold(rel) != folding.fold(&item.0))
        {
            flush(&mut group, &mut kept);
        }
        group.push(item);
    }
    flush(&mut group, &mut kept);
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    kept
}

struct Changed {
    rel: String,
    manifest: Manifest,
    stat: DiskStat,
    /// In an encrypted folder, each chunk's ID under the key it will be sealed with, worked out
    /// as the file is read rather than by reading it again under the folder lock (ADR 0023).
    chunk_ids: Vec<(tidefell_proto::ChunkHash, tidefell_proto::ChunkId)>,
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

/// True if `entry`'s current version is a file at `rel`: deleting the file there deletes it.
fn live_here(state: &FolderState, rel: &str, entry: EntryId) -> bool {
    state.meta.current(&entry).is_some_and(|v| {
        !matches!(v.kind, EntryKind::Tombstone)
            && state
                .meta
                .disk_path(&entry)
                .is_some_and(|p| state.same_place(rel, p))
    })
}

/// True if the file at `rel`, recorded as `entry`, being gone deletes `entry`: it is live, and
/// no other file on disk is recorded as it. The file need not be where `entry` goes now: a peer
/// may have moved it, or given its place to another entry, before this device moved the file,
/// and the user's delete still deletes it.
fn deletes(state: &FolderState, rel: &str, entry: EntryId) -> bool {
    let live = state
        .meta
        .current(&entry)
        .is_some_and(|v| !matches!(v.kind, EntryKind::Tombstone));
    live && state.local.with_entry(&entry).all(|(p, _)| p == rel)
}

/// True if a file at `rel` could be the file of an entry a peer sent: one whose size matches.
fn matchable(state: &FolderState, rel: &str, disk: &DiskStat) -> bool {
    state.meta.find_path(&recorded(rel)).is_some_and(|v| {
        matches!(v.kind, EntryKind::File { size, executable, .. }
            if *size == disk.size && disk.executable.is_none_or(|x| x == *executable))
    })
}

/// Scans `id` for changes since the last scan and records them: everything, or only `scope`.
/// `saved` is a file written through the engine, recorded even if it is ignored.
pub(crate) fn scan(
    inner: &Inner,
    handle: &FolderHandle,
    scope: Option<&Scope>,
    saved: Option<&str>,
) -> Result<Scanned> {
    let (root, rebuilding, exchanged) = {
        let mut state = handle.lock();
        // The caller checked the root is there, so what was kept can be found (ADR 0034).
        state.restore_if_rebuilding();
        (
            state.config.path.clone(),
            state.rebuilding(),
            state.exchanged(),
        )
    };

    let ignores = Ignores::load(&root);
    // The key an encrypted folder's changes are sealed with, if it has one yet.
    let seal_key = {
        let state = handle.lock();
        state
            .config
            .encryption
            .is_some()
            .then(|| crate::encrypted::newest_key(&state))
            .flatten()
    };
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
    let folding = handle.lock().folding();
    let mut found = one_per_path(found, handle, folding, &mut failures);
    if let Some(rel) = saved.filter(|rel| *rel != IGNORE_FILE && ignores.is_ignored(rel, false)) {
        if let Some(stat) = stat(&root.join(rel)) {
            if let Err(at) = found.binary_search_by(|(r, _)| r.as_str().cmp(rel)) {
                found.insert(at, (rel.to_owned(), stat));
            }
        }
    }

    // Each file is compared with what is recorded under the folder lock, briefly, rather than
    // with a copy of every record (ADR 0035).
    let mut changed = Vec::new();
    for (rel, disk) in &found {
        let (rel, disk) = (rel.clone(), *disk);
        let known = {
            let mut state = handle.lock();
            let known = state.local.get(&rel).map(|l| {
                l.size == disk.size
                    && l.mtime_ns == disk.mtime_ns
                    && disk.executable.is_none_or(|x| x == l.executable)
                    && (l.inode == 0 || l.inode == disk.inode)
            });
            // Inodes are not saved: a record read back learns its file's here.
            if known == Some(true) {
                state.local.set_inode(&rel, disk.inode);
            }
            known
        };
        if known == Some(true) {
            continue;
        }
        // While the folder is rebuilt, a file is only matched to an entry a peer sent for its
        // path, and one there is no such entry for yet is left alone: not read, not recorded,
        // and not a problem (ADR 0034).
        if rebuilding && known.is_none() && !matchable(&handle.lock(), &rel, &disk) {
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
        let mut chunk_ids = Vec::new();
        let read = chunk_reader(BufReader::new(file), Params::DEFAULT, |chunk, data| {
            if let Some(key) = &seal_key {
                chunk_ids.push((chunk.hash, key.chunk_id(data)));
            }
            Ok(())
        });
        let manifest = match read {
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
            chunk_ids,
        });
    }

    let mut state = handle.lock();
    state.ignores = ignores;
    state.touch();
    let mut index_changed = false;
    let mut conflicts = Vec::new();
    let now = inner.now_ms();
    // What is at the path of each file not recorded here, looked up before any change is
    // recorded: recording a version lays the folder out again (ADR 0024), so looking up after
    // each would make the first scan of a large folder take time in the square of its files. A
    // path that a change recorded below claims, as this device compares names, is looked up
    // again.
    let mut at_paths: HashMap<String, Option<(EntryId, EntryKind)>> = changed
        .iter()
        .filter(|c| state.local.get(&c.rel).is_none())
        .map(|c| {
            let path = recorded(&c.rel).into_owned();
            let at = state
                .meta
                .find_path(&path)
                .map(|v| (v.entry, v.kind.clone()));
            (path, at)
        })
        .collect();
    let mut claimed: HashSet<String> = HashSet::new();
    for c in changed {
        let content = c.manifest.content;
        // Recorded in NFC, whatever form the name has on disk (ADR 0033).
        let path = recorded(&c.rel).into_owned();
        let at_path =
            |state: &FolderState, at_paths: &mut HashMap<_, _>| match at_paths.remove(&path) {
                Some(at) if !claimed.contains(&*state.folding().fold(&path)) => at,
                _ => state
                    .meta
                    .find_path(&path)
                    .map(|v| (v.entry, v.kind.clone())),
            };
        let entry = match state.local.get(&c.rel).map(|l| l.entry) {
            Some(entry) => entry,
            None if rebuilding => match at_path(&state, &mut at_paths) {
                Some((
                    entry,
                    EntryKind::File {
                        content: c2,
                        executable,
                        ..
                    },
                )) if c2 == content && c.stat.executable.is_none_or(|x| x == executable) => entry,
                _ => continue,
            },
            // A file this device never had on disk here (a placeholder, a file dropped from
            // this device, or any file of a send-only folder) is the same entry only if it has
            // the same content. Otherwise it is a new file that happens to share the path, and
            // both are kept (ADR 0024) instead of it replacing the other device's file.
            None => match at_path(&state, &mut at_paths) {
                Some((entry, kind)) if file_content(&kind) == Some(content) => entry,
                _ => EntryId(random_bytes()?),
            },
        };
        // Where the filesystem has no executable bit, the entry keeps the one it has (ADR 0042).
        let executable = c.stat.executable.unwrap_or_else(|| {
            state.meta.current(&entry).is_some_and(|v| {
                matches!(
                    v.kind,
                    EntryKind::File {
                        executable: true,
                        ..
                    }
                )
            })
        });
        // The file is where its entry goes, by name as spelled apart from Unicode form: a
        // rename that only changes case is a change (ADR 0033).
        let up_to_date = state.meta.current(&entry).is_some_and(|v| {
            state.meta.disk_path(&entry) == Some(path.as_str())
                && matches!(v.kind, EntryKind::File { content: current, executable: x, .. }
                    if *current == content && *x == executable)
        });
        state.add_manifest(c.manifest);
        if let Some(key) = &seal_key {
            for (hash, id) in &c.chunk_ids {
                crate::encrypted::record_chunk_id(&mut state, *id, *hash, key.id());
            }
        }
        let base = if up_to_date {
            state.meta.current_vector(&entry)
        } else {
            // Build on the version the file had on disk, not on versions that arrived since and
            // were never written, so those become conflict copies instead of being lost.
            let base = base_of(&state, &c.rel, entry);
            let open = state.meta.open_conflicts(&entry).count();
            let path = recorded_path(&state, entry, &path);
            let kind = EntryKind::File {
                content,
                size: c.stat.size,
                executable,
            };
            let folding = state.folding();
            claimed.insert(folding.fold(&path));
            if let Some(was) = state.meta.disk_path(&entry) {
                claimed.insert(folding.fold(was));
            }
            let signer = crate::encrypted::signer(inner, &mut state, &kind, Some(&c.rel))?;
            let (_, vector) = state.meta.local_change_from(
                &signer,
                entry,
                &base,
                &path,
                kind,
                (c.stat.mtime_ns / 1_000_000) as i64,
            )?;
            if state.meta.open_conflicts(&entry).count() > open {
                conflicts.push(c.rel.clone());
            }
            index_changed = true;
            vector
        };
        // The same file under its old spelling, after a rename in case only (ADR 0033).
        let stale: Vec<String> = state
            .local
            .with_entry(&entry)
            .filter(|(k, _)| **k != *c.rel && state.same_place(k, &c.rel))
            .map(|(k, _)| k.to_string())
            .collect();
        for k in stale {
            state.local.remove(&k);
            state.used.remove(&k);
        }
        state.used.insert(c.rel.clone(), now);
        // A file put back by hand is on this device again.
        state.dropped.remove(&entry);
        let rel = c.rel.clone();
        state.record_local(
            c.rel,
            LocalFile {
                entry,
                content,
                size: c.stat.size,
                mtime_ns: c.stat.mtime_ns,
                executable,
                base,
            },
        );
        state.local.set_inode(&rel, c.stat.inode);
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
    // Nothing is deleted while the folder is rebuilt: what is recorded as on disk is only what
    // was matched, and the scan after the rebuild sees everything (ADR 0034).
    let mut deleted = Vec::new();
    let seen = |rel: &str| found.binary_search_by(|(r, _)| r.as_str().cmp(rel)).is_ok();
    for rel in state.local.keys() {
        if rebuilding
            || seen(rel)
            || !scope.is_none_or(|s| s.contains(rel))
            || state.ignores.is_ignored(rel, false)
            || unreadable
                .iter()
                .any(|dir| dir.is_empty() || rel.starts_with(&format!("{dir}/")))
        {
            continue;
        }
        match fs::symlink_metadata(root.join(&**rel)) {
            Ok(_) => {}
            Err(e) if is_gone(&e) => deleted.push(rel.to_string()),
            Err(e) => {
                if !failures.iter().any(|f| *f.path == **rel) {
                    warn!(path = %rel, error = %e, "cannot look at file");
                    failures.push(Failure::io(rel, Operation::Scan, &e));
                }
            }
        }
    }
    // Records are in no particular order; deletes are made in the order of their paths.
    deleted.sort_unstable();
    let deletes_here_only = state.config.sync.local_deletes == LocalDeletes::ThisDevice;
    // One scan that would delete much of the folder everywhere records none of it until the
    // user confirms (ADR 0037): a drive unmounted under the folder, or a mistake, looks the same.
    // The files stay recorded as on disk, so nothing is fetched back meanwhile.
    let everywhere: Vec<String> = deleted
        .iter()
        .filter(|rel| {
            !deletes_here_only
                && state
                    .local
                    .get(rel)
                    .is_some_and(|l| deletes(&state, rel, l.entry))
        })
        .cloned()
        .collect();
    let confirmed = std::mem::take(&mut state.confirm_deletes);
    if !confirmed
        && state
            .config
            .confirm_deletes
            .holds(everywhere.len(), state.local.len())
    {
        if state.held_deletes != everywhere {
            warn!(
                folder = %state.config.id,
                deletes = everywhere.len(),
                files = state.local.len(),
                "holding a scan's deletes until they are confirmed"
            );
            state.held_deletes = everywhere;
            inner.emit(crate::views::Event::FolderChanged(state.config.id));
        }
        deleted.clear();
    } else if (scope.is_none() || confirmed) && !state.held_deletes.is_empty() {
        state.held_deletes.clear();
        inner.emit(crate::views::Event::FolderChanged(state.config.id));
    }
    for rel in deleted {
        let deletes = state
            .local
            .get(&rel)
            .is_some_and(|l| deletes(&state, &rel, l.entry));
        let Some(local) = state.local.remove(&rel) else {
            continue;
        };
        state.used.remove(&rel);
        if deletes && deletes_here_only {
            // ADR 0025: dropped from this device only; it stays on every other device.
            debug!(path = %rel, "dropped from this device");
            state.dropped.insert(local.entry);
        } else if deletes {
            let base = if !local.base.is_empty() {
                local.base.clone()
            } else {
                state.meta.current_vector(&local.entry)
            };
            // A file not where its entry now goes is deleted at the entry's own path.
            let path = match live_here(&state, &rel, local.entry) {
                true => recorded_path(&state, local.entry, &recorded(&rel)),
                false => state
                    .meta
                    .current(&local.entry)
                    .map_or_else(|| recorded(&rel).into_owned(), |v| v.path.to_owned()),
            };
            let signer = crate::encrypted::signer(inner, &mut state, &EntryKind::Tombstone, None)?;
            state.meta.local_change_from(
                &signer,
                local.entry,
                &base,
                &path,
                EntryKind::Tombstone,
                inner.now_ms(),
            )?;
            index_changed = true;
        }
    }

    if rebuilding && state.finish_rebuild(inner.peers().is_empty(), exchanged, inner.now_ms()) {
        // The next scan records what was not matched as new files (ADR 0034).
        handle.rescan.notify_one();
        inner.emit(crate::views::Event::FolderChanged(state.config.id));
    }
    state.save()?;
    drop(state);
    if scope.is_none() {
        crate::content::forget_unreadable(inner, handle);
    }
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
        inner.now_ms(),
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
