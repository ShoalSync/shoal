//! Files: listing, opening, writing, deleting and moving them.

use std::path::{Path, PathBuf};

use tidefell_proto::sync::EntryKind;
use tidefell_proto::{EntryId, FolderId};

use super::{run_scan, Engine};
use crate::scan::Scope;
use crate::state::{recorded, FolderState};
use crate::views::{CopyInfo, Event, FileInfo};
use crate::{copies, evict, materialize, now_ms, Error, Result};

impl Engine {
    /// Live files in `folder`, sorted by path.
    pub fn files(&self, folder: FolderId) -> Result<Vec<FileInfo>> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        // Few devices make many files: look each name up once.
        let mut names: std::collections::HashMap<tidefell_proto::DeviceId, String> =
            std::collections::HashMap::new();
        let mut files: Vec<FileInfo> = state
            .meta
            .entries()
            .filter_map(|v| match v.kind {
                EntryKind::File {
                    content,
                    size,
                    executable,
                } => {
                    let path = state.meta.disk_path(&v.entry).unwrap_or(v.path);
                    Some(FileInfo {
                        entry: v.entry,
                        path: path.to_owned(),
                        size: *size,
                        modified_ms: v.modified_ms,
                        executable: *executable,
                        // On disk at its place, as this device tells names apart (ADR 0033).
                        on_disk: state
                            .local
                            .with_entry(&v.entry)
                            .any(|(p, l)| state.same_place(p, path) && l.content == *content),
                        conflicts: state.meta.open_conflicts(&v.entry).count(),
                        author: v.author(),
                        author_name: names
                            .entry(v.author())
                            .or_insert_with(|| self.device_name(&state, &v.author()))
                            .clone(),
                        confirmed_copies: state.copies.get(content).map_or(0, |d| d.len()),
                        only_here: copies::only_here(&state, self.inner.key.id(), v),
                        pinned: crate::pins::is_pinned(&state.pins, path),
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
        // Paths are recorded in NFC, whatever form the caller used (ADR 0033).
        let path = &*recorded(path);
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
            .get(content)
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
        // Paths are recorded in NFC, whatever form the caller used (ADR 0033).
        let path = &*recorded(path);
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let on_disk = state.meta.find_path(path).is_some_and(|v| match v.kind {
            EntryKind::File { content, .. } => state
                .local
                .get(&state.on_disk(path))
                .is_some_and(|l| l.entry == v.entry && l.content == *content),
            _ => false,
        });
        Ok(on_disk.then(|| state.config.path.join(state.on_disk(path))))
    }

    /// Makes the live file at `path` available on disk and returns where it is, fetching it
    /// from connected peers if this replica does not have its current version (a placeholder
    /// in a partial replica, or a file still pending in a full one). The file stays pinned,
    /// and so is never evicted, until a matching [`Engine::close`].
    pub async fn open(&self, folder: FolderId, path: &str) -> Result<PathBuf> {
        // Paths are recorded in NFC, whatever form the caller used (ADR 0033).
        let path = &*recorded(path);
        let handle = self.inner.folder(&folder)?;
        // Fetching takes the file lock itself once the content has arrived, so it is not held
        // here while waiting on peers (ADR 0024).
        let local = self.local_path(folder, path)?;
        if local.is_none() && self.inner.is_paused(&folder) {
            return Err(Error::Paused(folder));
        }
        if local.is_none()
            && !materialize::fetch_one(&self.inner, &self.node, folder, &handle, path).await?
        {
            return Err(Error::Unavailable(path.to_owned()));
        }
        let _guard = handle.disk().await?;
        // Evicted or replaced between fetching and holding it open; rare, and the caller can
        // retry.
        if self.local_path(folder, path)?.is_none() {
            return Err(Error::Unavailable(path.to_owned()));
        }
        // Kept open, and used, by the name the file has on disk.
        let disk = handle.lock().on_disk(path);
        *handle.open.lock().unwrap().entry(disk.clone()).or_insert(0) += 1;
        let abs = {
            let mut state = handle.lock();
            state.used.insert(disk.clone(), now_ms());
            state.config.path.join(&disk)
        };
        // Opening may have pushed a partial replica over its budget.
        evict::evict(&self.inner, folder, &handle)?;
        Ok(abs)
    }

    /// Pins the file or directory at `path` on a partial replica (ADR 0029): what it covers is
    /// fetched in the background, kept up to date and never evicted, and does not count against
    /// the folder's budget. Pins are this device's own and are not synced.
    pub async fn pin(&self, folder: FolderId, path: &str) -> Result<()> {
        // Paths are recorded in NFC, whatever form the caller used (ADR 0033).
        let path = &*recorded(path);
        let path = path.trim_end_matches('/');
        if !tidefell_proto::sync::is_valid_path(path) {
            return Err(Error::Invalid(format!("invalid path {path:?}")));
        }
        let handle = self.inner.folder(&folder)?;
        {
            let _guard = handle.disk().await?;
            let mut state = handle.lock();
            if !matches!(state.config.mode, crate::ReplicaMode::Partial { .. }) {
                return Err(Error::Invalid(
                    "only a partial replica can pin files; a full replica keeps every file".into(),
                ));
            }
            state.pins.insert(path.to_owned());
            state.touch();
        }
        self.save_pins()?;
        self.inner.emit(Event::FolderChanged(folder));
        // Fetch what is pinned now rather than at the next pass.
        self.inner.changed();
        Ok(())
    }

    /// Removes the pin at `path`. The files it covered become ordinary cached files again, and
    /// may be evicted to keep the folder within its budget.
    pub async fn unpin(&self, folder: FolderId, path: &str) -> Result<()> {
        // Paths are recorded in NFC, whatever form the caller used (ADR 0033).
        let path = &*recorded(path);
        let path = path.trim_end_matches('/');
        let handle = self.inner.folder(&folder)?;
        {
            let _guard = handle.disk().await?;
            let mut state = handle.lock();
            if !state.pins.remove(path) {
                return Err(Error::Invalid(format!("{path:?} is not pinned")));
            }
            state.touch();
            drop(state);
            self.save_pins()?;
            evict::evict(&self.inner, folder, &handle)?;
        }
        self.inner.emit(Event::FolderChanged(folder));
        Ok(())
    }

    /// Writes the pins to the runtime settings (ADR 0034).
    fn save_pins(&self) -> Result<()> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        self.inner.save_runtime()
    }

    /// The paths pinned on this device, in order.
    pub fn pins(&self, folder: FolderId) -> Result<Vec<String>> {
        let handle = self.inner.folder(&folder)?;
        let pins = handle.lock().pins.iter().cloned().collect();
        Ok(pins)
    }

    /// Releases one [`Engine::open`] of `path`.
    pub fn close(&self, folder: FolderId, path: &str) -> Result<()> {
        // Paths are recorded in NFC, whatever form the caller used (ADR 0033).
        let path = &*recorded(path);
        let handle = self.inner.folder(&folder)?;
        let disk = handle.lock().on_disk(path);
        let mut open = handle.open.lock().unwrap();
        if let Some(count) = open.get_mut(&disk) {
            *count -= 1;
            if *count == 0 {
                open.remove(&disk);
            }
        }
        Ok(())
    }

    /// Replaces or creates the file at `path` with the contents of `source`, which is moved
    /// (or copied, across filesystems) into the folder, and records the change now instead of
    /// at the next scan. It is recorded even at an ignored path: this is an explicit save, and
    /// a mounted folder shows only recorded files (ADR 0029).
    pub async fn write(&self, folder: FolderId, path: &str, source: &Path) -> Result<()> {
        // Paths are recorded in NFC, whatever form the caller used (ADR 0033).
        let path = &*recorded(path);
        if !tidefell_proto::sync::is_valid_path(path) {
            return Err(Error::Invalid(format!("invalid path {path:?}")));
        }
        let handle = self.inner.folder(&folder)?;
        {
            let _guard = handle.disk().await?;
            let (root, disk) = {
                let state = handle.lock();
                (state.config.path.clone(), state.on_disk(path))
            };
            let target = root.join(&disk);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(crate::io_err(parent))?;
            }
            // The recorded file being replaced, if it has no changes of its own: it becomes a
            // previous version (ADR 0029). Its record stays, so the scan below sees an edit.
            let replaced = handle
                .lock()
                .local
                .get(&disk)
                .filter(|l| crate::scan::unchanged(&target, l.size, l.mtime_ns))
                .map(|l| l.to_file());
            let tmp_dir = root.join(crate::scan::RESERVED_DIR).join("tmp");
            let tmp = tmp_dir.join(format!(
                "write-{:016x}",
                u64::from_le_bytes(crate::random_bytes()?)
            ));
            match replaced {
                Some(previous) => {
                    // The new content goes next to the folder first, so the swap is one
                    // filesystem's rename.
                    std::fs::create_dir_all(&tmp_dir).map_err(crate::io_err(&tmp_dir))?;
                    if crate::rename_durably(source, &tmp).is_err() {
                        std::fs::copy(source, &tmp).map_err(crate::io_err(source))?;
                    }
                    crate::history::keep(&mut handle.lock(), &disk, &previous, Some(&tmp))?;
                }
                None => {
                    if crate::rename_durably(source, &target).is_err() {
                        std::fs::create_dir_all(&tmp_dir).map_err(crate::io_err(&tmp_dir))?;
                        std::fs::copy(source, &tmp).map_err(crate::io_err(source))?;
                        crate::rename_durably(&tmp, &target).map_err(crate::io_err(&target))?;
                    }
                }
            }
        }
        // Only the saved file's directory needs a look.
        let disk = handle.lock().on_disk(path);
        let mut scope = Scope::default();
        scope.add(disk.rsplit_once('/').map_or("", |(dir, _)| dir), false);
        run_scan(&self.inner, folder, Some(scope), Some(disk))
            .await
            .map(drop)
    }

    /// Deletes the live file at `path`, sending it to peers' recycle bins. Works on placeholders
    /// too, which scans would otherwise never delete.
    pub async fn delete(&self, folder: FolderId, path: &str) -> Result<()> {
        // Paths are recorded in NFC, whatever form the caller used (ADR 0033).
        let path = &*recorded(path);
        let handle = self.inner.folder(&folder)?;
        {
            let _guard = handle.disk().await?;
            let mut state = handle.lock();
            let entry = match state.meta.find_path(path) {
                Some(v) if matches!(v.kind, EntryKind::File { .. }) => v.entry,
                _ => return Err(Error::NoSuchFile(path.to_owned())),
            };
            let root = state.config.path.clone();
            let disk = state.on_disk(path);
            // A delete of content on disk that is older than a version since received must not
            // supersede that version (ADR 0024).
            let base = crate::scan::base_of(&state, &disk, entry);
            remove_local(&mut state, &root, &disk)?;
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
        let (from, to) = (&*recorded(from), &*recorded(to));
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
            let _guard = handle.disk().await?;
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
                    let new_disk = state.on_disk(&new);
                    let base = crate::scan::base_of(&state, &new_disk, replaced);
                    remove_local(&mut state, &root, &new_disk)?;
                    state.meta.local_change_from(
                        key,
                        replaced,
                        &base,
                        &new,
                        EntryKind::Tombstone,
                        now,
                    )?;
                }
                // A file on disk moves with its content, so the move builds on that content's
                // base, which becomes the move's own vector (ADR 0029).
                let old = state.on_disk(&old);
                let base = crate::scan::base_of(&state, &old, entry);
                let (_, vector) =
                    state
                        .meta
                        .local_change_from(key, entry, &base, &new, kind, modified_ms)?;
                if let Some(mut local) = state.local.remove(&old) {
                    let (src, dst) = (root.join(&old), root.join(&new));
                    if let Some(parent) = dst.parent() {
                        std::fs::create_dir_all(parent).map_err(crate::io_err(parent))?;
                    }
                    crate::rename_durably(&src, &dst).map_err(crate::io_err(&src))?;
                    local.base = vector;
                    state.record_local(new.clone(), local);
                    if let Some(used) = state.used.remove(&old) {
                        state.used.insert(new.clone(), used);
                    }
                }
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
        if crate::scan::unchanged(&abs, local.size, local.mtime_ns) {
            crate::content::keep_deleted(state, &abs, local.content, None)?;
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
