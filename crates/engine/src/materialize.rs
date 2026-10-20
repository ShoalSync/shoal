//! Writing remote changes to disk.

use crate::engine::metrics::FetchFailure;
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use tidefell_fetch::{fetch, ChunkSource, Config};
use tidefell_net::{ChunkResult, EndpointId, Node};
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{
    ChunkHash, ChunkRef, ContentHash, DeviceId, EntryId, FolderId, Manifest, VersionVector,
};
use tracing::{debug, info, warn};

use crate::activity::Failure;
use crate::config::{Direction, ReplicaMode};
use crate::engine::{FolderHandle, Inner};
use crate::scan::{stat, unchanged, RESERVED_DIR};
use crate::state::{FolderState, LocalFile, NameFolding};
use crate::views::Event;
use crate::views::{Operation, ProblemKind};
use crate::{io_err, random_bytes, Error, Result};

/// Fetches chunks from connected peers over blobs/1.
struct PeerSource {
    node: Arc<Node>,
    folder: FolderId,
    /// To stop a fetch already running once its folder is paused (ADR 0029).
    inner: Arc<Inner>,
    /// Peers that are full replicas of the folder, so hold every current file (ADR 0032).
    full: Vec<DeviceId>,
}

impl PeerSource {
    /// Counts a failed request, and returns its error.
    fn failed(&self, reason: FetchFailure, error: impl std::fmt::Display) -> String {
        self.inner.counters.fetch_failed(reason);
        error.to_string()
    }

    fn check_paused(&self) -> std::result::Result<(), String> {
        match self.inner.is_paused(&self.folder) {
            true => Err(format!("syncing of folder {} is paused", self.folder)),
            false => Ok(()),
        }
    }
}

fn endpoint(peer: &DeviceId) -> std::result::Result<EndpointId, String> {
    EndpointId::from_bytes(peer.as_bytes()).map_err(|e| e.to_string())
}

impl ChunkSource for PeerSource {
    type Peer = DeviceId;

    async fn availability(
        &self,
        peer: &DeviceId,
        manifest: &Manifest,
    ) -> std::result::Result<Vec<ChunkHash>, String> {
        self.check_paused()?;
        let (count, bitmap) = self
            .node
            .availability(endpoint(peer)?, self.folder, manifest.content)
            .await
            .map_err(|e| self.failed(FetchFailure::Request, e))?;
        if count as usize != manifest.chunks.len() {
            return Ok(Vec::new());
        }
        Ok(manifest
            .chunks
            .iter()
            .enumerate()
            .filter(|(i, _)| bitmap[i / 8] & (1 << (i % 8)) != 0)
            .map(|(_, c)| c.hash)
            .collect())
    }

    fn holds_everything(&self, peer: &DeviceId) -> bool {
        self.full.contains(peer)
    }

    async fn fetch(
        &self,
        peer: &DeviceId,
        chunks: &[ChunkHash],
    ) -> std::result::Result<tidefell_fetch::Fetched, String> {
        self.check_paused()?;
        // Sent with other downloads' requests to the same peer (ADR 0032).
        let results = self
            .inner
            .batches
            .fetch(&self.node, *peer, self.folder, chunks.to_vec())
            .await
            .map_err(|e| self.failed(FetchFailure::Request, e))?;
        let mut fetched = tidefell_fetch::Fetched::default();
        for result in results {
            match result {
                ChunkResult::Found(hash, data) => fetched.chunks.push((hash, data)),
                ChunkResult::Missing(_) => {}
                ChunkResult::Busy(wait) => {
                    self.inner.counters.fetch_failed(FetchFailure::Busy);
                    fetched.busy_for = Some(wait);
                }
            }
        }
        // Chunks not sent count as missing, as the fetch treats them, unless the peer was busy.
        if fetched.busy_for.is_none() {
            for _ in fetched.chunks.len()..chunks.len() {
                self.inner.counters.fetch_failed(FetchFailure::Missing);
            }
        }
        Ok(fetched)
    }
}

pub(crate) struct PendingWrite {
    entry: EntryId,
    pub(crate) path: String,
    content: ContentHash,
    pub(crate) size: u64,
    executable: bool,
    modified_ms: i64,
    /// Where this entry was on disk before, if it moved.
    old_path: Option<String>,
    /// The vector of what is being written, taken when the write was planned: versions that
    /// arrive while it is fetched are not in it (ADR 0024).
    base: VersionVector,
    /// Why its content could not be fetched from the peers asked, if it could not (ADR 0030).
    unavailable: std::sync::Mutex<Option<String>>,
    /// Why it waits for space, if it does (ADR 0037).
    no_space: std::sync::Mutex<Option<String>>,
}

/// True for an error that is about the whole folder rather than one path.
fn folder_wide(error: &Error) -> bool {
    matches!(error, Error::FolderUnavailable(..) | Error::Crashed(_))
}

/// Files fetched and written at once (ADR 0032).
const MAX_WRITES: usize = 8;

/// The most file content fetched at once, by the sizes of the files being written; a larger
/// file is written on its own (ADR 0032).
const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The paths a write touches: its own and, if it moved, its previous one.
fn touched(write: &PendingWrite) -> impl Iterator<Item = &str> {
    std::iter::once(write.path.as_str()).chain(write.old_path.as_deref())
}

/// The folders above `path`, nearest first.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.char_indices()
        .rev()
        .filter(|(_, c)| *c == '/')
        .filter_map(|(i, _)| path.get(..i))
}

/// For each write, the earlier writes it must wait for: those touching the same path, or a
/// path inside or above one of its own, as this device tells names apart, so writes to one
/// place happen in plan order (ADR 0032, ADR 0033).
fn blockers(writes: &[Arc<PendingWrite>], folding: NameFolding) -> Vec<Vec<usize>> {
    // Earlier writes by each path they touch, and by each folder above those paths.
    let mut at: HashMap<String, Vec<usize>> = HashMap::new();
    let mut under: HashMap<String, Vec<usize>> = HashMap::new();
    let mut all = Vec::with_capacity(writes.len());
    for (i, write) in writes.iter().enumerate() {
        let paths: Vec<String> = touched(write).map(|p| folding.fold(p)).collect();
        let mut before: Vec<usize> = Vec::new();
        for path in &paths {
            before.extend(at.get(path).into_iter().flatten());
            before.extend(under.get(path).into_iter().flatten());
            for folder in ancestors(path) {
                before.extend(at.get(folder).into_iter().flatten());
            }
        }
        before.sort_unstable();
        before.dedup();
        all.push(before);
        for path in &paths {
            at.entry(path.clone()).or_default().push(i);
            for folder in ancestors(path) {
                under.entry(folder.to_owned()).or_default().push(i);
            }
        }
    }
    all
}

/// What materialising a folder would do now.
#[derive(Default)]
pub(crate) struct Plan {
    /// Files to write.
    pub(crate) writes: Vec<PendingWrite>,
    /// Paths to delete, as they are on disk.
    pub(crate) deletes: Vec<String>,
    /// Files whose names this folder's drive cannot store, and why (ADR 0033): not written,
    /// and listed as not on this device.
    pub(crate) refused: Vec<(String, String)>,
    /// Files already here under a name that differs from where they go only in case, as names
    /// on disk and where they go: renamed in place (ADR 0033).
    pub(crate) respell: Vec<(String, String)>,
}

/// What materialising `state` would do now. A send-only replica writes nothing (ADR 0025).
pub(crate) fn plan(state: &FolderState) -> Plan {
    let mut plan = Plan::default();
    if state.config.sync.direction == Direction::SendOnly {
        return plan;
    }
    // A partial replica keeps the files it has materialised up to date and leaves the rest
    // as placeholders.
    let partial = matches!(state.config.mode, ReplicaMode::Partial { .. });
    for version in state.meta.entries() {
        // The file on disk recorded as this entry; of two, as while a rename is applied, the
        // one with the later path.
        let here = state
            .local
            .with_entry(&version.entry)
            .max_by(|(a, _), (b, _)| a.cmp(b));
        let here = here.as_ref();
        match version.kind {
            EntryKind::File {
                content,
                size,
                executable,
            } => {
                // Where this entry goes on disk: its path, unless it shares it, also after this
                // device's folding (ADR 0024, ADR 0033).
                let path = state.meta.disk_path(&version.entry).unwrap_or(version.path);
                // A file on disk is at this path if its name is the same as this device tells
                // names apart: whatever Unicode form it has on disk, and whatever case on a
                // drive that ignores case (ADR 0033).
                let in_place = here.is_some_and(|(p, _)| state.same_place(p, path));
                let current = in_place
                    && here
                        .is_some_and(|(_, l)| l.content == *content && l.executable == *executable);
                // Files dropped from this device stay off it until opened (ADR 0025), and
                // ignored paths are never written (ADR 0029).
                let dropped = state.dropped.contains(&version.entry);
                let ignored = state.ignores.is_ignored(path, false);
                // A partial replica also keeps its pinned files (ADR 0029).
                let pinned = partial && crate::pins::is_pinned(&state.pins, path);
                let wanted = here.is_some() || pinned || (!partial && !dropped);
                if let Some((p, _)) = here.filter(|_| in_place) {
                    // Renamed elsewhere in case only: the file is renamed here too.
                    if crate::state::recorded(p) != path {
                        plan.respell.push(((*p).to_owned(), path.to_owned()));
                    }
                }
                if current || ignored || !wanted {
                    continue;
                }
                // A folder being rebuilt matches the file already at a path to its entry by
                // scanning it, rather than fetching content it may well have (ADR 0034).
                if here.is_none()
                    && state.rebuilding()
                    && fs::symlink_metadata(state.config.path.join(path)).is_ok()
                {
                    continue;
                }
                if let Some(reason) = state.filesystem.refuses(path).filter(|_| !in_place) {
                    plan.refused.push((path.to_owned(), reason));
                    continue;
                }
                plan.writes.push(PendingWrite {
                    entry: version.entry,
                    path: match here {
                        Some((p, _)) if in_place => (*p).to_owned(),
                        _ => path.to_owned(),
                    },
                    content: *content,
                    size: *size,
                    executable: *executable,
                    modified_ms: version.modified_ms,
                    old_path: here.map(|(p, _)| (*p).to_owned()).filter(|_| !in_place),
                    base: state.meta.current_vector(&version.entry),

                    unavailable: std::sync::Mutex::default(),
                    no_space: std::sync::Mutex::default(),
                });
            }
            EntryKind::Tombstone => {
                if let Some((p, _)) = here.filter(|(p, _)| !state.ignores.is_ignored(p, false)) {
                    plan.deletes.push((*p).to_owned());
                }
            }
            EntryKind::Directory | EntryKind::Symlink { .. } => {}
        }
    }
    plan
}

/// Brings the folder on disk up to date with its metadata, as far as content is available.
pub(crate) async fn materialize(
    inner: &Arc<Inner>,
    node: &Arc<Node>,
    id: FolderId,
    handle: &Arc<FolderHandle>,
) -> Result<()> {
    let (root, plan, fold) = {
        let mut state = handle.lock();
        // Files already on disk as their current version correspond to it (ADR 0029).
        state.catch_up_bases();
        // A send-only replica never applies other devices' changes to its files (ADR 0025).
        if state.config.sync.direction == Direction::SendOnly {
            return Ok(());
        }
        // Forget dropped entries that are no longer live, so the set does not grow forever.
        let dropped: Vec<EntryId> = state.dropped.iter().copied().collect();
        for entry in dropped {
            if state.meta.disk_path(&entry).is_none() {
                state.dropped.remove(&entry);
            }
        }
        let fold = state.folding();
        (state.config.path.clone(), plan(&state), fold)
    };
    let Plan {
        mut writes,
        mut deletes,
        refused,
        respell,
    } = plan;

    // One path that fails is recorded and the rest carry on (ADR 0030). One that failed in a
    // way that lasts keeps its problem without being tried again (ADR 0037).
    let mut failures = Vec::new();
    {
        let activity = handle.activity.lock().unwrap();
        let mut carry = |stuck: Option<Failure>| match stuck {
            Some(failure) => {
                failures.push(failure);
                false
            }
            None => true,
        };
        writes.retain(|w| carry(activity.stuck(&w.path, Operation::Write, Some(w.content))));
        deletes.retain(|path| carry(activity.stuck(path, Operation::Delete, None)));
    }
    if !deletes.is_empty() {
        let _guard = handle.disk().await?;
        for path in deletes {
            let failed = match remove_if_unchanged(inner, handle, id, &root, &path) {
                Ok(_) => None,
                Err(e) if folder_wide(&e) => return Err(e),
                Err(e) => {
                    warn!(folder = %id, %path, error = %e, "delete failed");
                    Some(Failure::error(&path, Operation::Delete, &e))
                }
            };
            handle
                .activity
                .lock()
                .unwrap()
                .tried(&path, Operation::Delete, None, failed.as_ref());
            failures.extend(failed);
        }
    }
    // A name this folder's drive cannot store is a problem, not a write that fails on every
    // pass (ADR 0033).
    for (path, reason) in refused {
        failures.push(Failure::new(
            &path,
            Operation::Write,
            ProblemKind::NameNotAllowed,
            reason,
        ));
    }
    if !respell.is_empty() {
        let _guard = handle.disk().await?;
        for (from, to) in respell {
            if let Err(e) = respell_file(handle, &root, &from, &to) {
                if folder_wide(&e) {
                    return Err(e);
                }
                warn!(folder = %id, path = %to, error = %e, "renaming in case failed");
                failures.push(Failure::error(&to, Operation::Write, &e));
            }
        }
    }
    // Several files at once, each fetched without the file lock (ADR 0032).
    let writes: Vec<Arc<PendingWrite>> = writes.into_iter().map(Arc::new).collect();
    let blockers = blockers(&writes, fold);
    let mut waiting_on: Vec<usize> = blockers.iter().map(Vec::len).collect();
    let mut unblocks: Vec<Vec<usize>> = vec![Vec::new(); writes.len()];
    for (i, before) in blockers.iter().enumerate() {
        for &j in before {
            if let Some(list) = unblocks.get_mut(j) {
                list.push(i);
            }
        }
    }
    // Writes free to start, in plan order.
    let mut ready: std::collections::BTreeSet<usize> = waiting_on
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == 0)
        .map(|(i, _)| i)
        .collect();
    let mut running = tokio::task::JoinSet::new();
    let mut bytes: u64 = 0;
    loop {
        let fits = |i: &usize, bytes: u64, idle: bool| {
            idle || writes
                .get(*i)
                .is_some_and(|w| bytes.saturating_add(w.size) <= MAX_BYTES)
        };
        while running.len() < MAX_WRITES {
            let Some(i) = ready
                .iter()
                .copied()
                .find(|i| fits(i, bytes, running.is_empty()))
            else {
                break;
            };
            ready.remove(&i);
            let Some(write) = writes.get(i).cloned() else {
                continue;
            };
            bytes = bytes.saturating_add(write.size);
            let (inner, node, handle, root) =
                (inner.clone(), node.clone(), handle.clone(), root.clone());
            running.spawn(async move {
                let result = write_file(&inner, &node, &handle, id, &root, &write).await;
                (i, result)
            });
        }
        let Some(joined) = running.join_next().await else {
            break;
        };
        let (i, result) = match joined {
            Ok(done) => done,
            // Cancelled, not panicked: the runtime is shutting down.
            Err(e) if e.is_cancelled() => {
                return Err(Error::FolderUnavailable(
                    id,
                    "the engine has shut down".into(),
                ));
            }
            Err(e) => {
                let message = inner.fail(
                    "write",
                    e.try_into_panic()
                        .unwrap_or_else(|e| Box::new(e.to_string())),
                );
                return Err(Error::Crashed(message));
            }
        };
        let Some(write) = writes.get(i) else {
            continue;
        };
        bytes = bytes.saturating_sub(write.size);
        for &next in unblocks.get(i).into_iter().flatten() {
            if let Some(n) = waiting_on.get_mut(next) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    ready.insert(next);
                }
            }
        }
        match result {
            Ok(true) => {
                handle.activity.lock().unwrap().tried(
                    &write.path,
                    Operation::Write,
                    Some(write.content),
                    None,
                );
                info!(folder = %id, path = %write.path, "updated");
                inner.emit(Event::FileUpdated {
                    folder: id,
                    path: write.path.clone(),
                });
            }
            Ok(false) => {
                if let Some(reason) = write.no_space.lock().unwrap().take() {
                    failures.push(Failure::new(
                        &write.path,
                        Operation::Write,
                        ProblemKind::DiskFull,
                        reason,
                    ));
                }
                if let Some(reason) = write.unavailable.lock().unwrap().take() {
                    failures.push(Failure::new(
                        &write.path,
                        Operation::Fetch,
                        ProblemKind::ContentUnavailable,
                        reason,
                    ));
                }
            }
            // An unmounted drive or a crash is the folder's problem, shown in its state, not
            // one per file. Dropping the set stops the other writes.
            Err(e) if folder_wide(&e) => return Err(e),
            Err(e) => {
                warn!(folder = %id, path = %write.path, error = %e, "update failed");
                let failure = Failure::error(&write.path, Operation::Write, &e);
                handle.activity.lock().unwrap().tried(
                    &write.path,
                    Operation::Write,
                    Some(write.content),
                    Some(&failure),
                );
                failures.push(failure);
            }
        }
    }
    // Each pass plans every write and delete, so problems of paths no longer planned, done,
    // deleted elsewhere or now ignored, go away.
    let changed = handle.activity.lock().unwrap().settle(
        &[Operation::Fetch, Operation::Write, Operation::Delete],
        |_| true,
        failures,
        inner.now_ms(),
    );
    if changed {
        inner.emit(Event::FolderChanged(id));
    }
    handle.lock().save()
}

/// Fetches and writes the current version of the live file at `path` now, for a front-end
/// opening it. Returns false if its content is not available from connected peers, or the file
/// on disk has local changes the next scan has not recorded yet.
pub(crate) async fn fetch_one(
    inner: &Arc<Inner>,
    node: &Arc<Node>,
    id: FolderId,
    handle: &FolderHandle,
    path: &str,
) -> Result<bool> {
    let (root, write) = {
        let state = handle.lock();
        let Some(version) = state.meta.find_path(path) else {
            return Err(Error::NoSuchFile(path.to_owned()));
        };
        let EntryKind::File {
            content,
            size,
            executable,
        } = version.kind
        else {
            return Err(Error::NoSuchFile(path.to_owned()));
        };
        let old_path = state
            .local
            .iter()
            .find(|(p, l)| l.entry == version.entry && !state.same_place(p, path))
            .map(|(p, _)| p.to_string());
        (
            state.config.path.clone(),
            PendingWrite {
                entry: version.entry,
                path: state.on_disk(path),
                content: *content,
                size: *size,
                executable: *executable,
                modified_ms: version.modified_ms,
                old_path,
                base: state.meta.current_vector(&version.entry),

                unavailable: std::sync::Mutex::default(),
                no_space: std::sync::Mutex::default(),
            },
        )
    };
    let written = write_file(inner, node, handle, id, &root, &write).await?;
    if written {
        inner.emit(Event::FileUpdated {
            folder: id,
            path: write.path.clone(),
        });
        handle.lock().save()?;
    }
    Ok(written)
}

/// Renames the file at `from` to `to`, which differ only in case, if it is still what the engine
/// last wrote: another device renamed it (ADR 0033).
fn respell_file(handle: &FolderHandle, root: &Path, from: &str, to: &str) -> Result<()> {
    let mut state = handle.lock();
    let Some((local, inode)) = state.local.get(from).map(|l| (l.to_file(), l.inode)) else {
        return Ok(());
    };
    let (src, dst) = (root.join(from), root.join(to));
    if !unchanged(&src, local.size, local.mtime_ns, 0) {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(io_err(parent))?;
    }
    crate::rename_durably(&src, &dst).map_err(io_err(&src))?;
    state.local.remove(from);
    state.record_local(to.to_owned(), local);
    // A rename keeps the file, and its inode.
    state.local.set_inode(to, inode);
    if let Some(used) = state.used.remove(from) {
        state.used.insert(to.to_owned(), used);
    }
    Ok(())
}

/// Removes `path` if it is still what the engine last wrote; forgets it either way. A file
/// edited locally is left in place and becomes a new entry at the next scan.
fn remove_if_unchanged(
    inner: &Inner,
    handle: &FolderHandle,
    id: FolderId,
    root: &Path,
    path: &str,
) -> Result<()> {
    let mut state = handle.lock();
    // The plan was made before the lock was taken: a local edit recorded since, which a
    // delete made apart from it does not supersede, keeps the file.
    let still_deleted = state.local.get(path).is_some_and(|local| {
        state
            .meta
            .current(&local.entry)
            .is_none_or(|v| matches!(v.kind, EntryKind::Tombstone))
    });
    if !still_deleted {
        return Ok(());
    }
    let inode = state.local.get(path).map_or(0, |l| l.inode);
    let Some(local) = state.local.remove(path) else {
        return Ok(());
    };
    state.used.remove(path);
    let abs = root.join(path);
    if unchanged(&abs, local.size, local.mtime_ns, inode) {
        let binned = state.meta.recycle_bin().get(&local.entry).is_some_and(|item| {
            matches!(item.version.kind, EntryKind::File { content, .. } if content == local.content)
        });
        if binned {
            // Moved aside, not copied, so the recycle bin keeps the content (ADR 0027).
            crate::content::keep_deleted(&mut state, &abs, local.content, local.entry, None)?;
        } else {
            // An older version than the one deleted, which never reached this device: it is a
            // previous version of the entry (ADR 0029), as it would be had the newer one come.
            crate::history::keep(&mut state, path, &local, None)?;
        }
        info!(folder = %id, path, "deleted");
        inner.emit(Event::FileRemoved {
            folder: id,
            path: path.to_owned(),
        });
    } else {
        debug!(folder = %id, path, "kept locally modified file that was deleted remotely");
    }
    Ok(())
}

/// True if `manifest` can describe a file with `content` and `size` (ADR 0024): its content
/// hash matches and its chunk lengths add up to the size. Whether the chunks really hash to the
/// content is only known once the file is assembled.
fn manifest_fits(manifest: &Manifest, content: ContentHash, size: u64) -> bool {
    manifest.content == content && manifest.len() == size
}

/// The manifest already here for `content`, if it fits the file. One that does not is dropped.
fn cached_manifest(handle: &FolderHandle, content: ContentHash, size: u64) -> Option<Manifest> {
    let mut state = handle.lock();
    let manifest = state.manifests.get(&content)?;
    if manifest_fits(&manifest, content, size) {
        return Some((*manifest).clone());
    }
    state.remove_manifest(&content);
    None
}

/// The manifest the reference chunker gives a file smaller than its smallest chunk: the whole
/// file as one chunk, whose hash is the content hash (ADR 0032). `None` for larger files.
fn single_chunk(content: ContentHash, size: u64) -> Option<Manifest> {
    let len = u32::try_from(size)
        .ok()
        .filter(|len| *len < tidefell_chunker::Params::DEFAULT.min)?;
    let chunks = match len {
        0 => Vec::new(),
        len => vec![ChunkRef {
            hash: ChunkHash(content.0),
            len,
        }],
    };
    Some(Manifest { content, chunks })
}

/// Asks connected peers for the manifest of `content`, one after another, and keeps the first
/// that fits.
async fn request_manifest(
    node: &Node,
    handle: &FolderHandle,
    id: FolderId,
    content: ContentHash,
    size: u64,
) -> Option<Manifest> {
    for peer in node.peers() {
        if !peer.shared_folders().contains(&id) {
            continue;
        }
        match peer.request_manifest(id, content).await {
            Ok(manifest) if manifest_fits(&manifest, content, size) => {
                handle.lock().add_manifest(manifest.clone());
                return Some(manifest);
            }
            Ok(_) => debug!(peer = %peer.device(), "manifest does not match the file's size"),
            Err(e) => debug!(peer = %peer.device(), error = %e, "manifest request failed"),
        }
    }
    None
}

/// Writes one file. Returns false if it could not be completed yet (content unavailable or
/// the target was modified locally).
async fn write_file(
    inner: &Arc<Inner>,
    node: &Arc<Node>,
    handle: &FolderHandle,
    id: FolderId,
    root: &Path,
    write: &PendingWrite,
) -> Result<bool> {
    // Only what fits above the free-space floors of the folder's volume and of the block
    // store's, where its chunks arrive first (ADR 0037).
    let room = inner
        .floors
        .room(root, write.size)
        .and_then(|()| inner.floors.room(&inner.data_dir, write.size));
    if let Err(short) = room {
        debug!(path = %write.path, %short, "not written for want of space");
        *write.no_space.lock().unwrap() = Some(short.to_string());
        return Ok(false);
    }
    let Some(manifest) = fetch_content(inner, node, handle, id, write).await? else {
        return Ok(false);
    };
    // Assembled below; the chunks stay in the block store until the next compaction.
    let _fetched = Fetching(inner, manifest.content);

    // Assemble into a temporary file and verify the whole content before it becomes visible.
    // That happens before taking the folder's file lock, like fetching, so neither a slow peer
    // nor a large file holds up scans, file operations or opening a placeholder (ADR 0024,
    // ADR 0032).
    let tmp_dir = root.join(RESERVED_DIR).join("tmp");
    fs::create_dir_all(&tmp_dir).map_err(io_err(&tmp_dir))?;
    let tmp = tmp_dir.join(format!(
        "{}-{:016x}",
        write.entry,
        u64::from_le_bytes(random_bytes()?)
    ));
    let assembled = assemble(inner, handle, &manifest, &tmp, write);
    let _guard = handle.disk().await?;
    // A chunk read from another file as it was being replaced or moved makes a bad copy or
    // none; under the lock no file changes, so a second failure is the real outcome.
    let assembled = match assembled {
        Ok(true) => Ok(true),
        _ => assemble(inner, handle, &manifest, &tmp, write),
    };
    match assembled {
        Ok(true) => {}
        Ok(false) => {
            // The manifest's chunks do not make up its content: drop it so the next attempt
            // fetches another instead of failing the same way forever.
            let _ = fs::remove_file(&tmp);
            handle.lock().remove_manifest(&write.content);
            debug!(path = %write.path, "manifest does not match its content; dropped");
            return Ok(false);
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            // A file the records say holds a chunk no longer does: it changed since the last
            // scan. Scanning again corrects the records, so the next attempt fetches the chunk
            // from a peer instead of failing the same way.
            handle.rescan.notify_one();
            return Err(e);
        }
    }

    let target = root.join(&write.path);
    // What is recorded is the file as written, before it takes its place: one changed there
    // after, even at once, differs from the record, so the next scan reads it.
    let written = stat(&tmp).ok_or_else(|| Error::Io {
        path: tmp.clone(),
        source: std::io::Error::other("file vanished after writing"),
    })?;
    let mut state = handle.lock();
    // The fetch ran without the lock: if the entry changed or moved meanwhile, as when another
    // entry takes or gives up the path its conflict copy name came from, the next pass writes
    // what is current. Placed regardless, the file would stay at a path the entry has left.
    let moved = state
        .meta
        .disk_path(&write.entry)
        .is_none_or(|p| !state.same_place(&write.path, p));
    if moved || state.meta.current_vector(&write.entry) != write.base {
        let _ = fs::remove_file(&tmp);
        debug!(path = %write.path, "changed while fetched; written at the next pass");
        return Ok(false);
    }
    // Never replace a file the user changed since the engine last saw it.
    let replaceable = match state.local.get(&write.path) {
        Some(local) => unchanged(&target, local.size, local.mtime_ns, local.inode),
        None => !target.exists(),
    };
    if !replaceable {
        let _ = fs::remove_file(&tmp);
        debug!(path = %write.path, "local changes pending; not overwriting");
        return Ok(false);
    }
    // The file there may be the only copy on this device of content still needed: another
    // entry's, about to move to its own path (ADR 0024), or a conflict copy. Move it aside
    // rather than overwrite it (ADR 0027). An older version of this entry goes into its history
    // (ADR 0029). Either way the new file takes its place with no moment where the path is
    // missing.
    let mut placed = false;
    if let Some(existing) = state.local.get(&write.path).map(|l| l.to_file()) {
        if existing.content != write.content {
            // A version that lost a conflict is kept for the recycle bin, which serves its
            // content to other devices, rather than only as this device's history (ADR 0029).
            let in_bin = state.meta.recycle_bin().iter().any(|r| {
                matches!(r.version.kind, EntryKind::File { content, .. } if content == existing.content)
            });
            let needed = existing.entry != write.entry
                || in_bin
                || state
                    .meta
                    .conflicts(&write.entry)
                    .iter()
                    .any(|c| matches!(c.version.kind, EntryKind::File { content, .. } if content == existing.content));
            if needed {
                state.local.remove(&write.path);
                let kept = crate::content::keep_deleted(
                    &mut state,
                    &target,
                    existing.content,
                    existing.entry,
                    Some(&tmp),
                );
                if let Err(e) = kept {
                    // The file changed or went since it was checked, as the user is free to do:
                    // still recorded, so the next scan finds what they did.
                    state.record_local(write.path.clone(), existing);
                    let _ = fs::remove_file(&tmp);
                    return Err(e);
                }
            } else {
                crate::history::keep(&mut state, &write.path, &existing, Some(&tmp))?;
            }
            placed = true;
        }
    }
    if !placed {
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_err(parent))?;
        }
        if state.local.contains_key(&write.path) {
            crate::rename_durably(&tmp, &target).map_err(io_err(&target))?;
        } else if !crate::rename_new_durably(&tmp, &target).map_err(io_err(&target))? {
            // A file the user put there since it was checked: theirs, recorded at the next scan.
            let _ = fs::remove_file(&tmp);
            debug!(path = %write.path, "a file appeared; not overwriting");
            return Ok(false);
        }
    }
    state.used.insert(write.path.clone(), inner.now_ms());
    state.dropped.remove(&write.entry);
    let disk = written;
    state.record_local(
        write.path.clone(),
        LocalFile {
            entry: write.entry,
            content: write.content,
            size: disk.size,
            mtime_ns: disk.mtime_ns,
            executable: disk.executable.unwrap_or(write.executable),
            base: write.base.clone(),
        },
    );
    state.local.set_inode(&write.path, disk.inode);
    // The file this entry was at before, unless another entry's file has taken that path since
    // the write was planned.
    let old_path = write
        .old_path
        .as_ref()
        .filter(|old| state.local.get(old).is_none_or(|l| l.entry == write.entry));
    if let Some(old) = old_path {
        state.used.remove(old);
        if let Some(local) = state.local.remove(old) {
            let abs = root.join(old);
            if unchanged(&abs, local.size, local.mtime_ns, 0) {
                if local.content == write.content {
                    let _ = fs::remove_file(&abs);
                } else {
                    // Moved and changed: the old content is a previous version (ADR 0029).
                    crate::history::keep(&mut state, old, &local, None)?;
                }
            }
        }
    }
    Ok(true)
}

/// Writes the content of `version`, a file version that need not be current, to `dest` as a
/// read-only file, fetching what is missing from connected peers (ADR 0029). Returns false if
/// the content is not available.
pub(crate) async fn write_copy(
    inner: &Arc<Inner>,
    node: &Arc<Node>,
    handle: &FolderHandle,
    id: FolderId,
    version: &tidefell_proto::sync::EntryVersion,
    dest: &Path,
) -> Result<bool> {
    let EntryKind::File {
        content,
        size,
        executable,
    } = version.kind
    else {
        return Ok(false);
    };
    let write = PendingWrite {
        entry: version.entry,
        path: version.path.clone(),
        content,
        size,
        executable,
        modified_ms: version.modified_ms,
        old_path: None,
        base: VersionVector::new(),

        unavailable: std::sync::Mutex::default(),
        no_space: std::sync::Mutex::default(),
    };
    let Some(manifest) = fetch_content(inner, node, handle, id, &write).await? else {
        return Ok(false);
    };
    let _fetched = Fetching(inner, manifest.content);
    let tmp = dest.with_extension("part");
    match assemble(inner, handle, &manifest, &tmp, &write) {
        Ok(true) => {}
        Ok(false) => {
            let _ = fs::remove_file(&tmp);
            handle.lock().remove_manifest(&write.content);
            return Ok(false);
        }
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
    }
    let mut permissions = fs::metadata(&tmp).map_err(io_err(&tmp))?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&tmp, permissions).map_err(io_err(&tmp))?;
    crate::rename_durably(&tmp, dest).map_err(io_err(dest))?;
    Ok(true)
}

/// Fetches the chunks of `write`'s content that are not here yet, and returns its manifest, or
/// `None` if the content is not available from connected peers.
async fn fetch_content(
    inner: &Arc<Inner>,
    node: &Arc<Node>,
    handle: &FolderHandle,
    id: FolderId,
    write: &PendingWrite,
) -> Result<Option<Manifest>> {
    let peers: Vec<DeviceId> = node
        .peers()
        .into_iter()
        .filter(|p| p.shared_folders().contains(&id))
        .map(|p| p.device())
        .collect();
    let unavailable = |reason: String| {
        if !peers.is_empty() && !inner.is_paused(&id) {
            *write.unavailable.lock().unwrap() = Some(reason);
        }
    };
    // An encrypted folder's content comes sealed over blobs/2 (ADR 0023).
    if handle.lock().config.encryption.is_some() {
        if let Some(manifest) = cached_manifest(handle, write.content, write.size) {
            if manifest
                .chunks
                .iter()
                .all(|c| crate::content::has_chunk(inner, &handle.lock(), &c.hash))
            {
                return Ok(Some(manifest));
            }
        }
        return match crate::encrypted::fetch_content(inner, node, handle, id, write.content).await {
            Ok(manifest) => Ok(Some(manifest)),
            Err(e) => {
                debug!(path = %write.path, error = %e, "content not available yet");
                *write.unavailable.lock().unwrap() = Some(e);
                Ok(None)
            }
        };
    }
    // A small file is one chunk named by its content hash, so it is fetched without asking for
    // its manifest first (ADR 0032). The manifest is recorded before the fetch, so this device
    // serves the chunk and compaction keeps it like any other.
    let (manifest, guessed) = match cached_manifest(handle, write.content, write.size) {
        Some(manifest) => (Some(manifest), false),
        None => match single_chunk(write.content, write.size) {
            Some(manifest) => {
                handle.lock().add_manifest(manifest.clone());
                (Some(manifest), true)
            }
            None => (
                request_manifest(node, handle, id, write.content, write.size).await,
                false,
            ),
        },
    };
    let Some(manifest) = manifest else {
        if !peers.is_empty() {
            inner.counters.fetch_failed(FetchFailure::Unavailable);
        }
        unavailable("no connected device has this version".to_owned());
        return Ok(None);
    };
    let error = match download(inner, node, handle, id, &manifest, &peers).await {
        Ok(()) => return Ok(Some(manifest)),
        Err(e) => e,
    };
    // Peers that split the file otherwise do not have that chunk: ask for theirs.
    if guessed && !inner.is_paused(&id) {
        handle.lock().remove_manifest(&write.content);
        if let Some(theirs) = request_manifest(node, handle, id, write.content, write.size).await {
            if theirs != manifest {
                return match download(inner, node, handle, id, &theirs, &peers).await {
                    Ok(()) => Ok(Some(theirs)),
                    Err(e) => {
                        debug!(path = %write.path, error = %e, "content not available yet");
                        unavailable(e);
                        Ok(None)
                    }
                };
            }
        }
    }
    debug!(path = %write.path, error = %error, "content not available yet");
    unavailable(error);
    Ok(None)
}

/// Fetches the chunks of `manifest` that are not here yet from `peers`.
async fn download(
    inner: &Arc<Inner>,
    node: &Arc<Node>,
    handle: &FolderHandle,
    id: FolderId,
    manifest: &Manifest,
    peers: &[DeviceId],
) -> std::result::Result<(), String> {
    // Chunks already here, in the block store or in any file on disk, are not fetched.
    let store_has = |h: &ChunkHash| crate::content::has_chunk(inner, &handle.lock(), h);
    if manifest.chunks.iter().all(|c| store_has(&c.hash)) {
        return Ok(());
    }
    inner.fetching.lock().unwrap().insert(manifest.content);
    // With one peer there is nothing to choose between, so whether it is a full replica does
    // not matter (ADR 0032).
    let mut full = Vec::new();
    if peers.len() > 1 {
        for peer in peers {
            if is_full_replica(inner, node, *peer, id).await {
                full.push(*peer);
            }
        }
    }
    let source = Arc::new(PeerSource {
        node: node.clone(),
        folder: id,
        inner: inner.clone(),
        full,
    });
    let download = fetch(
        source,
        manifest,
        peers,
        store_has,
        Config::default(),
        |hash, data| {
            inner
                .store
                .lock()
                .unwrap()
                .put_verified(hash, &data)
                .map_err(|e| e.to_string())?;
            inner.unreadable.lock().unwrap().remove(&hash);
            Ok(())
        },
    );
    // Pausing the folder stops the download: dropping it cancels the requests in flight.
    let fetched = tokio::select! {
        fetched = download => fetched.map(|_| ()).map_err(|e| {
            inner.counters.fetch_failed(match e {
                tidefell_fetch::FetchError::Unobtainable(_) => FetchFailure::Unavailable,
                tidefell_fetch::FetchError::Sink(_) => FetchFailure::Storage,
            });
            e.to_string()
        }),
        () = until_paused(inner, id) => Err("the folder was paused".to_owned()),
    };
    if fetched.is_err() {
        inner.fetching.lock().unwrap().remove(&manifest.content);
    }
    fetched
}

/// How long a peer's answer to whether it is a full replica is trusted. A stale one costs an
/// availability query, not a wrong file.
const REPLICA_ANSWER_KEPT: Duration = Duration::from_secs(600);

/// Whether `peer` is a full replica of `folder`, asked at most once per [`REPLICA_ANSWER_KEPT`]
/// (ADR 0032). A peer that cannot say counts as a partial replica.
async fn is_full_replica(inner: &Inner, node: &Node, peer: DeviceId, folder: FolderId) -> bool {
    let known = |inner: &Inner| {
        inner
            .full_replicas
            .lock()
            .unwrap()
            .get(&(peer, folder))
            .filter(|(_, at)| at.elapsed() < REPLICA_ANSWER_KEPT)
            .map(|(full, _)| *full)
    };
    if let Some(full) = known(inner) {
        return full;
    }
    let _asking = inner.replica_query.lock().await;
    if let Some(full) = known(inner) {
        return full;
    }
    let full = match EndpointId::from_bytes(peer.as_bytes()) {
        Ok(endpoint) => node
            .is_full_replica(endpoint, folder)
            .await
            .unwrap_or(false),
        Err(_) => false,
    };
    inner.remember_replica(peer, folder, full);
    full
}

/// Forgets that a content is being downloaded when dropped.
struct Fetching<'a>(&'a Inner, ContentHash);

impl Drop for Fetching<'_> {
    fn drop(&mut self) {
        self.0.fetching.lock().unwrap().remove(&self.1);
    }
}

/// Writes the file's content to `tmp`, from the block store and files on disk. Returns false if
/// the chunks do not hash to the content.
fn assemble(
    inner: &Inner,
    handle: &FolderHandle,
    manifest: &Manifest,
    tmp: &Path,
    write: &PendingWrite,
) -> Result<bool> {
    let mut file = fs::File::create(tmp).map_err(io_err(tmp))?;
    let mut hasher = blake3::Hasher::new();
    for chunk in &manifest.chunks {
        let data =
            crate::content::read_chunk(inner, handle, &chunk.hash).ok_or_else(|| Error::Io {
                path: tmp.to_path_buf(),
                source: std::io::Error::other("chunk no longer available during assembly"),
            })?;
        hasher.update(&data);
        file.write_all(&data).map_err(io_err(tmp))?;
    }
    if hasher.finalize().as_bytes() != manifest.content.as_bytes() || manifest.len() != write.size {
        return Ok(false);
    }
    file.sync_all().map_err(io_err(tmp))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if write.executable { 0o755 } else { 0o644 };
        fs::set_permissions(tmp, fs::Permissions::from_mode(mode)).map_err(io_err(tmp))?;
    }
    let mtime = UNIX_EPOCH + Duration::from_millis(write.modified_ms.max(0) as u64);
    file.set_modified(mtime).map_err(io_err(tmp))?;
    Ok(true)
}

/// Resolves once `folder`, or the whole device, is paused (ADR 0029).
async fn until_paused(inner: &Inner, folder: FolderId) {
    let mut changes = inner.changes.subscribe();
    while !inner.is_paused(&folder) {
        if changes.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(content: u8, lens: &[u32]) -> Manifest {
        Manifest {
            content: ContentHash([content; 32]),
            chunks: lens
                .iter()
                .map(|&len| ChunkRef {
                    hash: ChunkHash([len as u8; 32]),
                    len,
                })
                .collect(),
        }
    }

    fn pending(path: &str, old_path: Option<&str>) -> Arc<PendingWrite> {
        Arc::new(PendingWrite {
            entry: EntryId([path.len() as u8; 16]),
            path: path.to_owned(),
            content: ContentHash([0; 32]),
            size: 0,
            executable: false,
            modified_ms: 0,
            old_path: old_path.map(str::to_owned),
            base: VersionVector::new(),
            unavailable: std::sync::Mutex::default(),
            no_space: std::sync::Mutex::default(),
        })
    }

    #[test]
    fn on_a_drive_that_ignores_case_names_differing_in_case_are_one_place() {
        let writes = [
            pending("Photo.jpg", None),
            pending("photo.jpg", None),
            pending("Docs/a.txt", None),
            pending("docs", None),
        ];
        assert_eq!(
            blockers(&writes, NameFolding(tidefell_meta::Folding::Case)),
            [vec![], vec![0], vec![], vec![2]]
        );
        assert_eq!(
            blockers(&writes, NameFolding(tidefell_meta::Folding::None)),
            vec![Vec::<usize>::new(); 4]
        );
    }

    #[test]
    fn folders_above_a_path() {
        assert_eq!(ancestors("a/b/c.txt").collect::<Vec<_>>(), ["a/b", "a"]);
        assert_eq!(ancestors("c.txt").count(), 0);
    }

    #[test]
    fn writes_to_one_place_wait_for_earlier_ones() {
        let writes = [
            pending("a/x.txt", None),
            pending("b.txt", None),
            // The same path as 0.
            pending("a/x.txt", None),
            // A file where 0 and 2 have a folder.
            pending("a", None),
            // Moves 1 away: waits for it, and is waited for at both paths.
            pending("c.txt", Some("b.txt")),
            pending("b.txt", None),
            // Unrelated, and only a shared prefix with "a".
            pending("ab.txt", None),
            pending("a/y/z.txt", None),
        ];
        assert_eq!(
            blockers(&writes, NameFolding(tidefell_meta::Folding::None)),
            [
                vec![],
                vec![],
                vec![0],
                vec![0, 2],
                vec![1],
                vec![1, 4],
                vec![],
                vec![3],
            ]
        );
    }

    #[test]
    fn small_files_are_one_chunk_named_by_their_content() {
        let data = b"a small file";
        let content = ContentHash(*blake3::hash(data).as_bytes());
        let size = data.len() as u64;
        // What the chunker makes of the file.
        let chunked =
            tidefell_chunker::chunk_reader(&data[..], tidefell_chunker::Params::DEFAULT, |_, _| {
                Ok(())
            })
            .unwrap();
        assert_eq!(single_chunk(content, size), Some(chunked));
        assert_eq!(single_chunk(content, 0).unwrap().chunks, []);
        assert!(single_chunk(content, 64 * 1024 - 1).is_some());
        assert!(single_chunk(content, 64 * 1024).is_none());
    }

    #[test]
    fn manifest_must_fit_content_and_size() {
        let m = manifest(1, &[100, 50]);
        assert!(manifest_fits(&m, ContentHash([1; 32]), 150));
        assert!(
            !manifest_fits(&m, ContentHash([1; 32]), 151),
            "size differs"
        );
        assert!(
            !manifest_fits(&m, ContentHash([2; 32]), 150),
            "other content"
        );
        assert!(manifest_fits(&manifest(3, &[]), ContentHash([3; 32]), 0));
    }
}
