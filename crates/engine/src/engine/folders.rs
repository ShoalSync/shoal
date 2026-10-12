//! Folders: adding, removing and configuring them, and their status.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ContentHash, DeviceId, FolderId, UserId};
use tracing::info;

use super::recycle::recycle_summary;
use super::{open_folder, spawn_scanner, Engine};
use crate::config::{FolderConfig, NewFolder};
use crate::views::{Event, FolderInfo, FolderPeer, FolderState, PathProblem, RebuildInfo};
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
        // Connected peers and the folders each shares, for folder states (ADR 0030).
        // With when each session started: an exchange before it says nothing about now.
        let sessions: Vec<(DeviceId, BTreeSet<FolderId>, i64)> = self
            .node
            .peers()
            .into_iter()
            .map(|p| {
                let since = self
                    .node
                    .peer_status(&p.device())
                    .connected_since_ms
                    .unwrap_or(i64::MAX);
                (p.device(), p.shared_folders(), since)
            })
            .collect();
        self.inner
            .folders
            .read()
            .unwrap()
            .values()
            .map(|h| {
                let state = h.lock();
                let id = state.config.id;
                let writes = crate::materialize::plan(&state).writes;
                let pending_bytes = writes.iter().map(|w| w.size).sum();
                let activity = h.activity.lock().unwrap();
                let unavailable = h.unavailable.lock().unwrap().clone();
                let scan_error = activity
                    .last_scan
                    .as_ref()
                    .and_then(|(_, _, error)| error.clone());
                let save_error = state
                    .save_failed()
                    .map(|e| format!("saving the folder's metadata failed: {e}"));
                let error = unavailable.or(save_error).or(scan_error);
                let sharing: Vec<(DeviceId, i64)> = sessions
                    .iter()
                    .filter(|(_, folders, _)| folders.contains(&id))
                    .map(|(device, _, since)| (*device, *since))
                    .collect();
                let paused = self.inner.is_paused(&id);
                let rebuilding = state.rebuilding();
                let state_now = if error.is_some() {
                    FolderState::Error
                } else if rebuilding {
                    FolderState::Rebuilding
                } else if paused {
                    FolderState::Paused
                } else if activity.scanning {
                    FolderState::Scanning
                } else if !writes.is_empty() && !sharing.is_empty() {
                    FolderState::Syncing
                } else {
                    FolderState::Idle
                };
                let mut devices: BTreeSet<DeviceId> = sharing.iter().map(|(d, _)| *d).collect();
                devices.extend(activity.exchanged.keys().copied());
                let peers = devices
                    .into_iter()
                    .map(|device| {
                        let last_exchange_ms = activity.exchanged.get(&device).copied();
                        let session = sharing.iter().find(|(d, _)| *d == device);
                        FolderPeer {
                            device,
                            last_exchange_ms,
                            up_to_date: session.is_some_and(|(_, since)| {
                                last_exchange_ms.is_some_and(|at| at >= *since)
                            }),
                        }
                    })
                    .collect();
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
                    pinned_bytes: state
                        .meta
                        .entries()
                        .filter_map(|v| match v.kind {
                            EntryKind::File { size, .. } => Some((v, size)),
                            _ => None,
                        })
                        .filter(|(v, _)| {
                            let path = state.meta.disk_path(&v.entry).unwrap_or(&v.path);
                            crate::pins::is_pinned(&state.pins, path)
                        })
                        .map(|(_, size)| size)
                        .sum(),
                    conflicts: state
                        .meta
                        .entries()
                        .filter(|v| state.meta.open_conflicts(&v.entry).next().is_some())
                        .count(),
                    paused: self
                        .inner
                        .paused
                        .read()
                        .unwrap()
                        .1
                        .contains(&state.config.id),
                    state: state_now,
                    error,
                    pending_files: writes.len(),
                    pending_bytes,
                    last_scan_ms: activity.last_scan.as_ref().map(|(at, _, _)| *at),
                    last_scan_duration_ms: activity.last_scan.as_ref().map(|(_, took, _)| *took),
                    peers,
                    problems: activity.problems.len(),
                    held_deletes: state.held_deletes.len(),
                    low_space: low_space(&self.inner, &state),
                    rebuild: state.last_rebuild().map(|r| RebuildInfo {
                        started_ms: r.started_ms,
                        reason: r.reason.clone(),
                        finished_ms: r.finished_ms,
                        waiting_for: match rebuilding {
                            true => sharing.iter().map(|(d, _)| *d).collect(),
                            false => Vec::new(),
                        },
                    }),
                }
            })
            .collect()
    }

    /// The paths of `folder` that failed to scan, fetch, write or delete, until they succeed
    /// (ADR 0030).
    pub fn path_problems(&self, folder: FolderId) -> Result<Vec<PathProblem>> {
        let handle = self.inner.folder(&folder)?;
        let activity = handle.activity.lock().unwrap();
        Ok(activity.problems.values().cloned().collect())
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
        if self.inner.folders.read().unwrap().contains_key(&id)
            || self.inner.unopened.lock().unwrap().contains_key(&id)
        {
            return Err(Error::FolderExists(id));
        }
        if config.members.is_empty() {
            return Err(Error::Invalid("a folder needs at least one member".into()));
        }
        if config.path.as_os_str().is_empty() {
            return Err(Error::Invalid("a folder needs a path".into()));
        }
        let handle = Arc::new(open_folder(
            &self.inner.data_dir,
            &self.inner.metadata_dir,
            config,
            self.id(),
            false,
            None,
        )?);
        self.inner
            .folders
            .write()
            .unwrap()
            .insert(id, handle.clone());
        self.inner.watch_commits(&handle);
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
            confirm_deletes: Default::default(),
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

    /// Starts `id` over (ADR 0034): its metadata is set aside, files on disk are matched to what
    /// peers send, and the recycle bin and history are restored from what was kept. For a folder
    /// whose metadata is wrong in a way the engine cannot see.
    pub async fn rebuild_folder(&self, id: FolderId) -> Result<()> {
        super::rebuild_folder(&self.inner, id, "it was asked for").await?;
        info!(folder = %id, "folder rebuilding");
        Ok(())
    }

    /// Folders that could not be opened when the engine started, and why (ADR 0034).
    pub fn unopened_folders(&self) -> Vec<(FolderId, String)> {
        let unopened = self.inner.unopened.lock().unwrap();
        unopened
            .iter()
            .map(|(id, (_, _, e))| (*id, e.clone()))
            .collect()
    }

    /// The files a scan found deleted and held, because there were so many (ADR 0037).
    pub fn held_deletes(&self, id: FolderId) -> Result<Vec<String>> {
        Ok(self.inner.folder(&id)?.lock().held_deletes.clone())
    }

    /// Records the deletes a scan held (ADR 0037): the files are deleted on every device, each
    /// keeping them in its recycle bin. A scan runs now and records them.
    pub async fn confirm_deletes(&self, id: FolderId) -> Result<()> {
        self.inner.folder(&id)?.lock().confirm_deletes = true;
        info!(folder = %id, "deletes confirmed");
        self.scan(id).await
    }

    /// Brings back the files a scan found deleted and held (ADR 0037): they are fetched again
    /// from other devices, and nothing is deleted anywhere.
    pub async fn restore_deleted(&self, id: FolderId) -> Result<()> {
        let handle = self.inner.folder(&id)?;
        {
            let _guard = handle.disk().await?;
            let mut state = handle.lock();
            for path in std::mem::take(&mut state.held_deletes) {
                // Not on disk, as far as the engine knows: materialising fetches it again.
                state.local.remove(&path);
                state.used.remove(&path);
            }
            state.save()?;
        }
        info!(folder = %id, "deleted files restored");
        self.inner.emit(Event::FolderChanged(id));
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
        for version in state.meta.entries() {
            if state.meta.open_conflicts(&version.entry).next().is_some() {
                status.conflicts += 1;
            }
            if let EntryKind::File { content, .. } = version.kind {
                let path = state
                    .meta
                    .disk_path(&version.entry)
                    .unwrap_or(&version.path);
                status.files.insert(path.to_owned(), content);
            }
        }
        // What materialising would write now (ADR 0030).
        status.pending = crate::materialize::plan(&state).writes.len();
        Ok(status)
    }
}

/// Why `state`'s folder takes no downloads for want of space, if it does (ADR 0037).
fn low_space(inner: &crate::engine::Inner, state: &crate::state::FolderState) -> Option<String> {
    let root = &state.config.path;
    let (short, place) = match inner.floors.low(root) {
        Some(short) => (short, "the folder's drive"),
        None => (
            inner.floors.low(&inner.data_dir)?,
            "the drive holding Tidefell's data",
        ),
    };
    let kept = recycle_summary(state).bytes;
    Some(format!(
        "{place} is low on space ({short}); this folder's recycle bin and earlier versions hold {} here",
        crate::space::size(kept)
    ))
}
