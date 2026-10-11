//! Files removed from this device only, kept for the recycle bin (ADR 0025).
//!
//! Removing a file here moves it into `<folder>/.tidefell/recycle/` and drops its entry from
//! this device: other devices keep it, and it is not downloaded again until opened. The file
//! waits there until the folder's retention expires or the recycle bin is emptied, and
//! restoring it moves it back.

use std::fs;
use std::path::{Path, PathBuf};

use tidefell_proto::EntryId;
use tracing::{debug, warn};

use crate::scan::{stat, unchanged, RESERVED_DIR};
use crate::state::{FolderState, SetAside};
use crate::views::RecycledInfo;
use crate::{io_err, random_bytes, Error, Result};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn recycle_dir(root: &Path) -> PathBuf {
    root.join(RESERVED_DIR).join("recycle")
}

fn file_path(root: &Path, item: &SetAside) -> PathBuf {
    recycle_dir(root).join(&item.file)
}

/// Moves the live file at `path` into the recycle directory and drops its entry from this
/// device. Returns what was set aside. Callers hold the folder's file lock.
pub(crate) fn remove_here(state: &mut FolderState, path: &str, now_ms: i64) -> Result<SetAside> {
    let root = state.config.path.clone();
    let local = state
        .local
        .get(path)
        .cloned()
        .ok_or_else(|| Error::NoSuchFile(path.to_owned()))?;
    if !state
        .meta
        .disk_path(&local.entry)
        .is_some_and(|p| state.same_place(path, p))
    {
        return Err(Error::NoSuchFile(path.to_owned()));
    }
    let abs = root.join(path);
    if !unchanged(&abs, &local) {
        return Err(Error::LocalChanges(path.to_owned()));
    }
    let dir = recycle_dir(&root);
    fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    // Keeps Android's media scanner out of the engine's own files.
    let nomedia = root.join(RESERVED_DIR).join(".nomedia");
    if !nomedia.exists() {
        fs::write(&nomedia, b"").map_err(io_err(&nomedia))?;
    }
    let item = SetAside {
        path: path.to_owned(),
        file: format!(
            "{}-{:016x}",
            local.entry,
            u64::from_le_bytes(random_bytes()?)
        ),
        local,
        removed_at_ms: now_ms,
    };
    let dest = file_path(&root, &item);
    crate::rename_durably(&abs, &dest).map_err(io_err(&abs))?;
    let entry = item.local.entry;
    if let Some(older) = state.set_aside.insert(entry, item.clone()) {
        remove_file(&root, &older);
    }
    state.local.remove(path);
    state.used.remove(path);
    state.dropped.insert(entry);
    Ok(item)
}

/// Moves the set-aside file of `entry` back to where its entry is now, and puts the entry back
/// on this device. Returns the path it was restored to.
pub(crate) fn restore(state: &mut FolderState, entry: &EntryId) -> Result<String> {
    let root = state.config.path.clone();
    let item = state
        .set_aside
        .get(entry)
        .cloned()
        .ok_or(Error::NotRecycled(*entry))?;
    // Where the entry is now: it may have moved on another device since.
    let path = state
        .meta
        .disk_path(entry)
        .map(str::to_owned)
        .ok_or(Error::NotRecycled(*entry))?;
    let target = root.join(&path);
    if state.local.contains_key(&path) || target.exists() {
        return Err(Error::PathTaken(path));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(io_err(parent))?;
    }
    let source = file_path(&root, &item);
    crate::rename_durably(&source, &target).map_err(io_err(&source))?;
    let disk = stat(&target).ok_or_else(|| Error::Io {
        path: target.clone(),
        source: std::io::Error::other("file vanished after restoring"),
    })?;
    // The content is what it was when removed; a later version of the entry makes the next
    // scan record it as a local change built on the old one, so nothing is lost (ADR 0024).
    let mut local = item.local;
    local.size = disk.size;
    local.mtime_ns = disk.mtime_ns;
    local.executable = disk.executable;
    state.set_aside.remove(entry);
    state.dropped.remove(entry);
    state.local.insert(path.clone(), local);
    state.used.insert(path.clone(), crate::now_ms());
    Ok(path)
}

/// When a set-aside item expires under the folder's retention, or `None` to keep it forever.
pub(crate) fn expires_at_ms(state: &FolderState, item: &SetAside) -> Option<i64> {
    state
        .config
        .retention
        .days()
        .map(|days| item.removed_at_ms.saturating_add(i64::from(days) * DAY_MS))
}

/// The set-aside items as recycle-bin entries.
pub(crate) fn infos(state: &FolderState) -> impl Iterator<Item = RecycledInfo> + '_ {
    state.set_aside.values().map(|item| RecycledInfo {
        entry: item.local.entry,
        path: item.path.clone(),
        size: item.local.size,
        deleted_at_ms: item.removed_at_ms,
        expires_at_ms: expires_at_ms(state, item),
        here_only: true,
        conflict: false,
    })
}

/// Deletes the set-aside file of `entry`, or of every item if `None`, for good. Returns the
/// removed items.
pub(crate) fn discard(state: &mut FolderState, entry: Option<&EntryId>) -> Vec<SetAside> {
    let root = state.config.path.clone();
    let removed: Vec<SetAside> = match entry {
        Some(entry) => state.set_aside.remove(entry).into_iter().collect(),
        None => state.set_aside.take_all().into_values().collect(),
    };
    for item in &removed {
        remove_file(&root, item);
    }
    removed
}

/// Deletes set-aside files whose retention expired at or before `now_ms`, and those whose
/// entry is no longer live: deleted everywhere, the synced recycle bin keeps it instead.
/// Returns the removed items.
pub(crate) fn purge(state: &mut FolderState, now_ms: i64) -> Vec<SetAside> {
    let expired: Vec<EntryId> = state
        .set_aside
        .iter()
        .filter(|(entry, item)| {
            expires_at_ms(state, item).is_some_and(|at| at <= now_ms)
                || state.meta.disk_path(entry).is_none()
        })
        .map(|(entry, _)| *entry)
        .collect();
    expired
        .iter()
        .flat_map(|entry| discard(state, Some(entry)))
        .collect()
}

fn remove_file(root: &Path, item: &SetAside) {
    let path = file_path(root, item);
    match fs::remove_file(&path) {
        Ok(()) => debug!(path = %item.path, "removed set-aside file"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(path = %path.display(), error = %e, "cannot remove set-aside file"),
    }
}
