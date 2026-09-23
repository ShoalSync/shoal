//! Detecting local changes by scanning a folder on disk.

use std::collections::BTreeSet;
use std::fs;
use std::io::BufReader;
use std::path::Path;

use tidefell_chunker::{chunk_reader, Params};
use tidefell_proto::sync::{is_valid_path, EntryKind};
use tidefell_proto::{EntryId, Manifest};
use tracing::{debug, warn};

use crate::engine::{FolderHandle, Inner};
use crate::state::LocalFile;
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
        let manifest = chunk_reader(BufReader::new(file), Params::DEFAULT, |chunk, bytes| {
            inner
                .store
                .lock()
                .unwrap()
                .put_verified(chunk.hash, bytes)
                .map_err(std::io::Error::other)
        })
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
    for c in changed {
        let entry = match state.local.get(&c.rel).map(|l| l.entry) {
            Some(entry) => entry,
            None => match state.meta.find_path(&c.rel) {
                Some(v) => v.entry,
                None => EntryId(random_bytes()?),
            },
        };
        let content = c.manifest.content;
        let up_to_date = state.meta.current(&entry).is_some_and(|v| {
            v.path == c.rel
                && matches!(v.kind, EntryKind::File { content: current, executable, .. }
                    if current == content && executable == c.stat.executable)
        });
        state.add_manifest(c.manifest);
        if !up_to_date {
            state.meta.local_change(
                inner.key.signing_key(),
                entry,
                &c.rel,
                EntryKind::File {
                    content,
                    size: c.stat.size,
                    executable: c.stat.executable,
                },
                (c.stat.mtime_ns / 1_000_000) as i64,
            )?;
            index_changed = true;
        }
        state.local.insert(
            c.rel,
            LocalFile {
                entry,
                content,
                size: c.stat.size,
                mtime_ns: c.stat.mtime_ns,
                executable: c.stat.executable,
            },
        );
    }

    let deleted: Vec<String> = known
        .keys()
        .filter(|rel| !seen.contains(*rel))
        .cloned()
        .collect();
    for rel in deleted {
        let Some(local) = state.local.remove(&rel) else {
            continue;
        };
        let live_here = state
            .meta
            .current(&local.entry)
            .is_some_and(|v| v.path == rel && !matches!(v.kind, EntryKind::Tombstone));
        if live_here {
            state.meta.local_change(
                inner.key.signing_key(),
                local.entry,
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
