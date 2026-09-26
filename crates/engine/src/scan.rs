//! Detecting local changes by scanning a folder on disk.

use std::collections::BTreeSet;
use std::fs;
use std::io::BufReader;
use std::path::Path;

use tidefell_chunker::{chunk_reader, Params};
use tidefell_proto::sync::{is_valid_path, EntryKind};
use tidefell_proto::{EntryId, Manifest, VersionVector};
use tracing::{debug, warn};

use crate::config::LocalDeletes;
use crate::engine::{FolderHandle, Inner};
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
        Some(local) if local.entry == entry && local.base.iter().next().is_some() => {
            local.base.clone()
        }
        _ => state.meta.known_vector(&entry),
    }
}

pub(crate) fn unchanged(path: &Path, local: &LocalFile) -> bool {
    stat(path).is_some_and(|s| s.size == local.size && s.mtime_ns == local.mtime_ns)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, DiskStat)>) {
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
        if file_type.is_dir() {
            walk(root, &path, out);
        } else if file_type.is_file() {
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
            if !is_valid_path(&rel) {
                warn!(path = %rel, "skipping path that cannot be synced");
                continue;
            }
            if let Some(stat) = stat(&path) {
                out.push((rel, stat));
            }
        }
        // Symlinks and special files are not synced yet.
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

/// Scans `id` for changes since the last scan and records them. Returns true if the index
/// changed.
pub(crate) fn scan(inner: &Inner, handle: &FolderHandle) -> Result<bool> {
    let (root, known) = {
        let state = handle.lock();
        (state.config.path.clone(), state.local.clone())
    };

    let mut found = Vec::new();
    walk(&root, &root, &mut found);

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
    let mut index_changed = false;
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
            state.meta.known_vector(&entry)
        } else {
            // Build on the version the file had on disk, not on versions that arrived since and
            // were never written, so those become conflict copies instead of being lost.
            let base = base_of(&state, &c.rel, entry);
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

    let deleted: Vec<String> = known
        .keys()
        .filter(|rel| !seen.contains(*rel))
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
            let base = if local.base.iter().next().is_some() {
                local.base.clone()
            } else {
                state.meta.known_vector(&local.entry)
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
    Ok(index_changed)
}
