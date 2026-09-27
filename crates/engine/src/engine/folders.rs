//! Folders: adding, removing and configuring them, and their status.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ContentHash, EntryId, FolderId, UserId};
use tracing::info;

use super::recycle::recycle_summary;
use super::{open_folder, spawn_scanner, Engine};
use crate::config::{FolderConfig, NewFolder};
use crate::views::{Event, FolderInfo};
use crate::{copies, Error, Result};

/// A summary of one folder, for status displays and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderStatus {
    /// Live files by path, with their content hash.
    pub files: BTreeMap<String, ContentHash>,
    /// Live files not yet written to disk on this replica.
    pub pending: usize,
    /// Entries with unresolved conflict copies.
    pub conflicts: usize,
    /// Paths of entries in the recycle bin.
    pub recycled: Vec<String>,
}

impl Engine {
    /// All folders.
    pub fn folders(&self) -> Vec<FolderInfo> {
        self.inner
            .folders
            .read()
            .unwrap()
            .values()
            .map(|h| {
                let state = h.lock();
                FolderInfo {
                    id: state.config.id,
                    path: state.config.path.clone(),
                    mode: state.config.mode,
                    members: state.config.members.clone(),
                    retention: state.config.retention,
                    sync: state.config.sync,
                    recycle_bin: recycle_summary(&state),
                    configured: h.configured,
                    only_here: state
                        .meta
                        .entries()
                        .filter(|v| copies::only_here(&state, self.inner.key.id(), v))
                        .count(),
                    watch_error: h.watch_error.lock().unwrap().clone(),
                    unavailable: h.unavailable.lock().unwrap().clone(),
                    paused: self
                        .inner
                        .paused
                        .read()
                        .unwrap()
                        .1
                        .contains(&state.config.id),
                }
            })
            .collect()
    }

    /// Pauses syncing of `folder`, or of the whole device if `None` (ADR 0029). A paused folder
    /// still scans, so local changes are recorded, but nothing is sent or received for it. It
    /// stays paused across restarts.
    pub fn pause(&self, folder: Option<FolderId>) -> Result<()> {
        self.set_paused(folder, true)
    }

    /// Resumes syncing of `folder`, or of the whole device if `None`.
    pub fn resume(&self, folder: Option<FolderId>) -> Result<()> {
        self.set_paused(folder, false)
    }

    /// Whether syncing is paused for the whole device.
    pub fn paused(&self) -> bool {
        self.inner.paused.read().unwrap().0
    }

    fn set_paused(&self, folder: Option<FolderId>, paused: bool) -> Result<()> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        if let Some(id) = folder {
            self.inner.folder(&id)?;
        }
        {
            let mut state = self.inner.paused.write().unwrap();
            match folder {
                None => state.0 = paused,
                Some(id) if paused => {
                    state.1.insert(id);
                }
                Some(id) => {
                    state.1.remove(&id);
                }
            }
        }
        self.inner.save_runtime()?;
        info!(folder = ?folder, paused, "sync pause changed");
        match folder {
            Some(id) => self.inner.emit(Event::FolderChanged(id)),
            None => {
                for id in self.inner.folder_ids() {
                    self.inner.emit(Event::FolderChanged(id));
                }
            }
        }
        // Sessions offer or withdraw the folders; materialising picks up again on resume.
        self.inner.changed();
        Ok(())
    }

    /// Adds a folder at runtime: a new one (with a fresh ID) or one shared from another device.
    /// It is remembered across restarts.
    pub fn add_folder(&self, config: FolderConfig) -> Result<()> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        let id = config.id;
        if self.inner.folders.read().unwrap().contains_key(&id) {
            return Err(Error::FolderExists(id));
        }
        if config.members.is_empty() {
            return Err(Error::Invalid("a folder needs at least one member".into()));
        }
        if config.path.as_os_str().is_empty() {
            return Err(Error::Invalid("a folder needs a path".into()));
        }
        let handle = Arc::new(open_folder(&self.inner.data_dir, config, self.id(), false)?);
        self.inner
            .folders
            .write()
            .unwrap()
            .insert(id, handle.clone());
        self.inner.save_runtime()?;
        spawn_scanner(&self.inner, id, &handle);
        info!(folder = %id, "folder added");
        self.inner.emit(Event::FolderAdded(id));
        // Offer the new folder to connected peers on their next session.
        self.inner.changed();
        Ok(())
    }

    /// Adds `new` as [`Engine::add_folder`] does, after filling in its defaults, and returns
    /// the added folder.
    pub fn create_folder(&self, new: NewFolder) -> Result<FolderInfo> {
        let id = match new.id {
            Some(id) => id,
            None => FolderId(crate::random_bytes()?),
        };
        let members = if new.members.is_empty() {
            vec![self.user()]
        } else {
            new.members
        };
        self.add_folder(FolderConfig {
            id,
            path: new.path,
            mode: new.mode,
            members,
            retention: new.retention,
            scan_interval: new.scan_interval,
            sync: new.sync,
        })?;
        self.folders()
            .into_iter()
            .find(|f| f.id == id)
            .ok_or(Error::UnknownFolder(id))
    }

    /// Stops syncing a folder added at runtime and forgets its metadata. Files on disk are
    /// left in place.
    pub fn remove_folder(&self, id: FolderId) -> Result<()> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        let handle = self.inner.folder(&id)?;
        if handle.configured {
            return Err(Error::Configured(id));
        }
        self.inner.folders.write().unwrap().remove(&id);
        // A folder joined again later starts unpaused.
        self.inner.paused.write().unwrap().1.remove(&id);
        if let Some(scanner) = handle.scanner.lock().unwrap().take() {
            scanner.abort();
        }
        self.inner.save_runtime()?;
        // Taking the state lock waits for any save in progress; later saves are skipped.
        let dir = handle.lock().mark_removed();
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(crate::io_err(&dir))?;
        }
        info!(folder = %id, "folder removed");
        self.inner.emit(Event::FolderRemoved(id));
        // Sessions stop offering the folder.
        self.inner.changed();
        Ok(())
    }

    /// Changes how long this replica keeps deleted files. Items past a shortened retention
    /// are purged at the next hourly purge, not immediately. Other replicas keep their own
    /// retention.
    pub fn set_retention(&self, id: FolderId, retention: crate::Retention) -> Result<()> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        let handle = self.inner.folder(&id)?;
        if handle.configured {
            return Err(Error::Configured(id));
        }
        {
            let mut state = handle.lock();
            if state.config.retention == retention {
                return Ok(());
            }
            state.set_retention(retention);
            state.save()?;
        }
        self.inner.save_runtime()?;
        info!(folder = %id, ?retention, "retention changed");
        self.inner.emit(Event::FolderChanged(id));
        Ok(())
    }

    /// Changes how `id` syncs on this device (ADR 0025). Files already dropped from this device
    /// stay dropped. Other replicas are not affected.
    pub fn set_sync_options(&self, id: FolderId, sync: crate::SyncOptions) -> Result<()> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        let handle = self.inner.folder(&id)?;
        if handle.configured {
            return Err(Error::Configured(id));
        }
        {
            let mut state = handle.lock();
            if state.config.sync == sync {
                return Ok(());
            }
            state.config.sync = sync;
        }
        self.inner.save_runtime()?;
        info!(folder = %id, ?sync, "sync options changed");
        self.inner.emit(Event::FolderChanged(id));
        // Sessions report the replica kind again (ADR 0022), and writes resume or stop.
        self.inner.changed();
        Ok(())
    }

    fn change_members(
        &self,
        id: FolderId,
        change: impl FnOnce(&mut Vec<UserId>) -> bool,
    ) -> Result<bool> {
        let _guard = self.inner.runtime_lock.lock().unwrap();
        let handle = self.inner.folder(&id)?;
        if handle.configured {
            return Err(Error::Configured(id));
        }
        let changed = {
            let mut state = handle.lock();
            let mut members = state.config.members.clone();
            let changed = change(&mut members);
            if members.is_empty() {
                return Err(Error::Invalid("a folder needs at least one member".into()));
            }
            if changed {
                state.set_members(members);
            }
            changed
        };
        if changed {
            self.inner.save_runtime()?;
            self.inner.emit(Event::FolderChanged(id));
            // Sessions re-offer folders to peers whose access changed.
            self.inner.changed();
        }
        Ok(changed)
    }

    /// Lets `user`'s devices sync `folder`. Returns false if already a member.
    pub fn add_member(&self, folder: FolderId, user: UserId) -> Result<bool> {
        self.change_members(folder, |m| {
            if m.contains(&user) {
                false
            } else {
                m.push(user);
                true
            }
        })
    }

    /// Stops `user`'s devices syncing `folder` from now on. Returns false if not a member.
    pub fn remove_member(&self, folder: FolderId, user: UserId) -> Result<bool> {
        self.change_members(folder, |m| {
            let before = m.len();
            m.retain(|u| *u != user);
            m.len() != before
        })
    }

    /// A summary of `folder`.
    pub fn status(&self, folder: FolderId) -> Result<FolderStatus> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let mut status = FolderStatus {
            files: BTreeMap::new(),
            pending: 0,
            conflicts: 0,
            recycled: state
                .meta
                .recycle_bin()
                .iter()
                .map(|r| r.version.path.clone())
                .chain(state.set_aside.values().map(|s| s.path.clone()))
                .collect(),
        };
        let partial = matches!(state.config.mode, crate::ReplicaMode::Partial { .. });
        let send_only = state.config.sync.direction == crate::Direction::SendOnly;
        let materialised: HashSet<EntryId> = state.local.values().map(|l| l.entry).collect();
        for version in state.meta.entries() {
            if !state.meta.conflicts(&version.entry).is_empty() {
                status.conflicts += 1;
            }
            if let EntryKind::File { content, .. } = version.kind {
                let path = state
                    .meta
                    .disk_path(&version.entry)
                    .unwrap_or(&version.path);
                status.files.insert(path.to_owned(), content);
                let on_disk = state
                    .local
                    .get(path)
                    .is_some_and(|l| l.entry == version.entry && l.content == content);
                // Placeholders of a partial replica, files dropped from this device, ignored
                // paths and every file of a send-only replica are not pending.
                let dropped = state.dropped.contains(&version.entry);
                if !on_disk
                    && !send_only
                    && !state.ignores.is_ignored(path, false)
                    && (materialised.contains(&version.entry) || (!partial && !dropped))
                {
                    status.pending += 1;
                }
            }
        }
        Ok(status)
    }
}
