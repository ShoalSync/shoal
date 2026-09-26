//! Files: listing, opening, writing, deleting and moving them.

use std::path::{Path, PathBuf};

use tidefell_proto::sync::EntryKind;
use tidefell_proto::{EntryId, FolderId};

use super::{run_scan, Engine};
use crate::state::FolderState;
use crate::views::{CopyInfo, Event, FileInfo};
use crate::{copies, evict, materialize, now_ms, Error, Result};

impl Engine {
    /// Live files in `folder`, sorted by path.
    pub fn files(&self, folder: FolderId) -> Result<Vec<FileInfo>> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let mut files: Vec<FileInfo> = state
            .meta
            .entries()
            .filter_map(|v| match v.kind {
                EntryKind::File {
                    content,
                    size,
                    executable,
                } => {
                    let path = state.meta.disk_path(&v.entry).unwrap_or(&v.path);
                    Some(FileInfo {
                        entry: v.entry,
                        path: path.to_owned(),
                        size,
                        modified_ms: v.modified_ms,
                        executable,
                        on_disk: state
                            .local
                            .get(path)
                            .is_some_and(|l| l.entry == v.entry && l.content == content),
                        conflicts: state.meta.conflicts(&v.entry).len(),
                        author: v.author,
                        confirmed_copies: state.copies.get(&content).map_or(0, |d| d.len()),
                        only_here: copies::only_here(&state, self.inner.key.id(), v),
                    })
                }
                _ => None,
            })
            .collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(files)
    }

    /// The full replicas that confirmed holding the current version of `path` (ADR 0022).
    /// Empty for files made on other devices, which are not tracked.
    pub fn copies(&self, folder: FolderId, path: &str) -> Result<Vec<CopyInfo>> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let content = state
            .meta
            .find_path(path)
            .and_then(|v| match v.kind {
                EntryKind::File { content, .. } => Some(content),
                _ => None,
            })
            .ok_or_else(|| Error::NoSuchFile(path.to_owned()))?;
        Ok(state
            .copies
            .get(&content)
            .map(|devices| {
                devices
                    .iter()
                    .map(|(device, at)| CopyInfo {
                        device: *device,
                        name: state.copy_names.get(device).cloned().unwrap_or_default(),
                        confirmed_ms: *at,
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Where a live file of `folder` is on this device's disk, if its current version is there.
    pub fn local_path(&self, folder: FolderId, path: &str) -> Result<Option<PathBuf>> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let on_disk = state.meta.find_path(path).is_some_and(|v| match v.kind {
            EntryKind::File { content, .. } => state
                .local
                .get(path)
                .is_some_and(|l| l.entry == v.entry && l.content == content),
            _ => false,
        });
        Ok(on_disk.then(|| state.config.path.join(path)))
    }

    /// Makes the live file at `path` available on disk and returns where it is, fetching it
    /// from connected peers if this replica does not have its current version (a placeholder
    /// in a partial replica, or a file still pending in a full one). The file stays pinned,
    /// and so is never evicted, until a matching [`Engine::close`].
    pub async fn open(&self, folder: FolderId, path: &str) -> Result<PathBuf> {
        let handle = self.inner.folder(&folder)?;
        // Fetching takes the file lock itself once the content has arrived, so it is not held
        // here while waiting on peers (ADR 0024).
        if self.local_path(folder, path)?.is_none()
            && !materialize::fetch_one(&self.inner, &self.node, folder, &handle, path).await?
        {
            return Err(Error::Unavailable(path.to_owned()));
        }
        let _guard = handle.fs_lock.lock().await;
        // Evicted or replaced between fetching and pinning; rare, and the caller can retry.
        if self.local_path(folder, path)?.is_none() {
            return Err(Error::Unavailable(path.to_owned()));
        }
        *handle
            .pins
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_insert(0) += 1;
        let abs = {
            let mut state = handle.lock();
            state.used.insert(path.to_owned(), now_ms());
            state.config.path.join(path)
        };
        // Opening may have pushed a partial replica over its budget.
        evict::evict(&self.inner, folder, &handle)?;
        Ok(abs)
    }

    /// Releases one [`Engine::open`] of `path`.
    pub fn close(&self, folder: FolderId, path: &str) -> Result<()> {
        let handle = self.inner.folder(&folder)?;
        let mut pins = handle.pins.lock().unwrap();
        if let Some(count) = pins.get_mut(path) {
            *count -= 1;
            if *count == 0 {
                pins.remove(path);
            }
        }
        Ok(())
    }

    /// Replaces or creates the file at `path` with the contents of `source`, which is moved
    /// (or copied, across filesystems) into the folder, and records the change now instead of
    /// at the next scan.
    pub async fn write(&self, folder: FolderId, path: &str, source: &Path) -> Result<()> {
        if !tidefell_proto::sync::is_valid_path(path) {
            return Err(Error::Invalid(format!("invalid path {path:?}")));
        }
        let handle = self.inner.folder(&folder)?;
        {
            let _guard = handle.fs_lock.lock().await;
            let root = handle.lock().config.path.clone();
            let target = root.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(crate::io_err(parent))?;
            }
            if std::fs::rename(source, &target).is_err() {
                let tmp_dir = root.join(crate::scan::RESERVED_DIR).join("tmp");
                std::fs::create_dir_all(&tmp_dir).map_err(crate::io_err(&tmp_dir))?;
                let tmp = tmp_dir.join(format!(
                    "write-{:016x}",
                    u64::from_le_bytes(crate::random_bytes()?)
                ));
                std::fs::copy(source, &tmp).map_err(crate::io_err(source))?;
                std::fs::rename(&tmp, &target).map_err(crate::io_err(&target))?;
            }
        }
        run_scan(&self.inner, folder).await
    }

    /// Deletes the live file at `path`, sending it to peers' recycle bins. Works on placeholders
    /// too, which scans would otherwise never delete.
    pub async fn delete(&self, folder: FolderId, path: &str) -> Result<()> {
        let handle = self.inner.folder(&folder)?;
        {
            let _guard = handle.fs_lock.lock().await;
            let mut state = handle.lock();
            let entry = match state.meta.find_path(path) {
                Some(v) if matches!(v.kind, EntryKind::File { .. }) => v.entry,
                _ => return Err(Error::NoSuchFile(path.to_owned())),
            };
            let root = state.config.path.clone();
            // A delete of content on disk that is older than a version since received must not
            // supersede that version (ADR 0024).
            let base = crate::scan::base_of(&state, path, entry);
            remove_local(&mut state, &root, path)?;
            state.meta.local_change_from(
                self.inner.key.signing_key(),
                entry,
                &base,
                path,
                EntryKind::Tombstone,
                now_ms(),
            )?;
            state.save()?;
        }
        self.inner.changes.send_modify(|n| *n += 1);
        self.inner.emit(Event::FileRemoved {
            folder,
            path: path.to_owned(),
        });
        self.inner.emit(Event::FolderChanged(folder));
        Ok(())
    }

    /// Moves the live file at `from`, or every live file under the directory `from`, to `to`.
    /// Entries keep their identity and content, so a placeholder moves without being fetched.
    /// A live file already at a destination is deleted first.
    pub async fn rename(&self, folder: FolderId, from: &str, to: &str) -> Result<()> {
        if !tidefell_proto::sync::is_valid_path(to) {
            return Err(Error::Invalid(format!("invalid path {to:?}")));
        }
        if from == to {
            return Ok(());
        }
        if to.starts_with(&format!("{from}/")) {
            return Err(Error::Invalid(format!("cannot move {from:?} into itself")));
        }
        let handle = self.inner.folder(&folder)?;
        {
            let _guard = handle.fs_lock.lock().await;
            let mut state = handle.lock();
            let prefix = format!("{from}/");
            // Paths are where files are on disk, which for entries that share a path is not
            // their recorded path (ADR 0024).
            let moves: Vec<(EntryId, String, String, EntryKind, i64)> = state
                .meta
                .entries()
                .filter(|v| matches!(v.kind, EntryKind::File { .. }))
                .filter_map(|v| {
                    let old = state.meta.disk_path(&v.entry)?;
                    let dest = if old == from {
                        to.to_owned()
                    } else {
                        format!("{to}/{}", old.strip_prefix(&prefix)?)
                    };
                    Some((v.entry, old.to_owned(), dest, v.kind.clone(), v.modified_ms))
                })
                .collect();
            if moves.is_empty() {
                return Err(Error::NoSuchFile(from.to_owned()));
            }
            if let Some((_, _, dest, _, _)) = moves
                .iter()
                .find(|(_, _, dest, _, _)| !tidefell_proto::sync::is_valid_path(dest))
            {
                return Err(Error::Invalid(format!("invalid path {dest:?}")));
            }
            let root = state.config.path.clone();
            let key = self.inner.key.signing_key();
            let now = now_ms();
            for (entry, old, new, kind, modified_ms) in moves {
                // Replace whatever is at the destination.
                let replaced = state
                    .meta
                    .find_path(&new)
                    .filter(|v| v.entry != entry && matches!(v.kind, EntryKind::File { .. }))
                    .map(|v| v.entry);
                if let Some(replaced) = replaced {
                    remove_local(&mut state, &root, &new)?;
                    state
                        .meta
                        .local_change(key, replaced, &new, EntryKind::Tombstone, now)?;
                }
                if let Some(local) = state.local.remove(&old) {
                    let (src, dst) = (root.join(&old), root.join(&new));
                    if let Some(parent) = dst.parent() {
                        std::fs::create_dir_all(parent).map_err(crate::io_err(parent))?;
                    }
                    std::fs::rename(&src, &dst).map_err(crate::io_err(&src))?;
                    state.local.insert(new.clone(), local);
                    if let Some(used) = state.used.remove(&old) {
                        state.used.insert(new.clone(), used);
                    }
                }
                state
                    .meta
                    .local_change(key, entry, &new, kind, modified_ms)?;
            }
            state.save()?;
        }
        self.inner.changes.send_modify(|n| *n += 1);
        self.inner.emit(Event::FolderChanged(folder));
        Ok(())
    }
}

/// Removes the file at `path` from disk and forgets it, if this replica has it. Its content is
/// kept for the recycle bin if the file is unchanged since it was recorded (ADR 0027).
fn remove_local(state: &mut FolderState, root: &Path, path: &str) -> Result<()> {
    if let Some(local) = state.local.remove(path) {
        let abs = root.join(path);
        if crate::scan::unchanged(&abs, &local) {
            crate::content::keep_deleted(state, &abs, local.content)?;
        } else {
            match std::fs::remove_file(&abs) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(crate::io_err(&abs)(e)),
            }
        }
    }
    state.used.remove(path);
    Ok(())
}
