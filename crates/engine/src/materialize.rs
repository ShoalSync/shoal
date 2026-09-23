//! Writing remote changes to disk.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use tidefell_fetch::{fetch, ChunkSource, Config};
use tidefell_net::{ChunkResult, EndpointId, Node};
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ChunkHash, ContentHash, DeviceId, EntryId, FolderId, Manifest};
use tracing::{debug, info, warn};

use crate::config::ReplicaMode;
use crate::engine::{FolderHandle, Inner};
use crate::scan::{stat, unchanged, RESERVED_DIR};
use crate::state::LocalFile;
use crate::views::Event;
use crate::{io_err, random_bytes, Error, Result};

/// Fetches chunks from connected peers over blobs/1.
struct PeerSource {
    node: Arc<Node>,
    folder: FolderId,
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
        let (count, bitmap) = self
            .node
            .availability(endpoint(peer)?, self.folder, manifest.content)
            .await
            .map_err(|e| e.to_string())?;
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

    async fn fetch(
        &self,
        peer: &DeviceId,
        chunks: &[ChunkHash],
    ) -> std::result::Result<Vec<(ChunkHash, Vec<u8>)>, String> {
        let results = self
            .node
            .fetch_chunks(endpoint(peer)?, self.folder, chunks)
            .await
            .map_err(|e| e.to_string())?;
        Ok(results
            .into_iter()
            .filter_map(|r| match r {
                ChunkResult::Found(hash, data) => Some((hash, data)),
                ChunkResult::Missing(_) => None,
            })
            .collect())
    }
}

struct PendingWrite {
    entry: EntryId,
    path: String,
    content: ContentHash,
    size: u64,
    executable: bool,
    modified_ms: i64,
    /// Where this entry was on disk before, if it moved.
    old_path: Option<String>,
}

/// Brings the folder on disk up to date with its metadata, as far as content is available.
pub(crate) async fn materialize(
    inner: &Arc<Inner>,
    node: &Arc<Node>,
    id: FolderId,
    handle: &FolderHandle,
) -> Result<()> {
    let (root, writes, deletes, mode) = {
        let state = handle.lock();
        let on_disk: HashMap<EntryId, (&String, &LocalFile)> =
            state.local.iter().map(|(p, l)| (l.entry, (p, l))).collect();
        let mut writes = Vec::new();
        let mut deletes = Vec::new();
        for version in state.meta.entries() {
            let here = on_disk.get(&version.entry);
            match version.kind {
                EntryKind::File {
                    content,
                    size,
                    executable,
                } => {
                    let current = here.is_some_and(|(p, l)| {
                        **p == version.path && l.content == content && l.executable == executable
                    });
                    if !current {
                        writes.push(PendingWrite {
                            entry: version.entry,
                            path: version.path.clone(),
                            content,
                            size,
                            executable,
                            modified_ms: version.modified_ms,
                            old_path: here
                                .map(|(p, _)| (*p).clone())
                                .filter(|p| *p != version.path),
                        });
                    }
                }
                EntryKind::Tombstone => {
                    if let Some((p, _)) = here {
                        deletes.push((*p).clone());
                    }
                }
                EntryKind::Directory | EntryKind::Symlink { .. } => {}
            }
        }
        (
            state.config.path.clone(),
            writes,
            deletes,
            state.config.mode,
        )
    };

    for path in deletes {
        remove_if_unchanged(inner, handle, id, &root, &path)?;
    }
    if matches!(mode, ReplicaMode::Partial { .. }) {
        return Ok(());
    }
    for write in writes {
        match write_file(inner, node, handle, id, &root, &write).await {
            Ok(true) => {
                info!(folder = %id, path = %write.path, "updated");
                inner.emit(Event::FileUpdated {
                    folder: id,
                    path: write.path.clone(),
                });
            }
            Ok(false) => {}
            Err(e) => warn!(folder = %id, path = %write.path, error = %e, "update failed"),
        }
    }
    handle.lock().save()
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
    let Some(local) = state.local.remove(path) else {
        return Ok(());
    };
    let abs = root.join(path);
    if unchanged(&abs, &local) {
        fs::remove_file(&abs).map_err(io_err(&abs))?;
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

async fn manifest_for(
    node: &Node,
    handle: &FolderHandle,
    id: FolderId,
    content: ContentHash,
) -> Result<Option<Manifest>> {
    if let Some(m) = handle.lock().manifests.get(&content) {
        return Ok(Some(m.clone()));
    }
    for peer in node.peers() {
        if !peer.shared_folders().contains(&id) {
            continue;
        }
        match peer.request_manifest(id, content).await {
            Ok(manifest) => {
                handle.lock().add_manifest(manifest.clone());
                return Ok(Some(manifest));
            }
            Err(e) => debug!(peer = %peer.device(), error = %e, "manifest request failed"),
        }
    }
    Ok(None)
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
    let Some(manifest) = manifest_for(node, handle, id, write.content).await? else {
        return Ok(false);
    };

    let peers: Vec<DeviceId> = node
        .peers()
        .into_iter()
        .filter(|p| p.shared_folders().contains(&id))
        .map(|p| p.device())
        .collect();
    let store_has = |h: &ChunkHash| inner.store.lock().unwrap().contains(h);
    if manifest.chunks.iter().any(|c| !store_has(&c.hash)) {
        let source = Arc::new(PeerSource {
            node: node.clone(),
            folder: id,
        });
        let fetched = fetch(
            source,
            &manifest,
            &peers,
            store_has,
            Config::default(),
            |hash, data| {
                inner
                    .store
                    .lock()
                    .unwrap()
                    .put_verified(hash, &data)
                    .map_err(|e| e.to_string())
            },
        )
        .await;
        if let Err(e) = fetched {
            debug!(path = %write.path, error = %e, "content not available yet");
            return Ok(false);
        }
    }

    // Assemble into a temporary file and verify the whole content before it becomes visible.
    let tmp_dir = root.join(RESERVED_DIR).join("tmp");
    fs::create_dir_all(&tmp_dir).map_err(io_err(&tmp_dir))?;
    let tmp = tmp_dir.join(format!(
        "{}-{:016x}",
        write.entry,
        u64::from_le_bytes(random_bytes()?)
    ));
    let result = assemble(inner, &manifest, &tmp, write);
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    let target = root.join(&write.path);
    let mut state = handle.lock();
    // Never replace a file the user changed since the engine last saw it.
    let replaceable = match state.local.get(&write.path) {
        Some(local) => unchanged(&target, local),
        None => !target.exists(),
    };
    if !replaceable {
        let _ = fs::remove_file(&tmp);
        debug!(path = %write.path, "local changes pending; not overwriting");
        return Ok(false);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(io_err(parent))?;
    }
    fs::rename(&tmp, &target).map_err(io_err(&target))?;
    let disk = stat(&target).ok_or_else(|| Error::Io {
        path: target.clone(),
        source: std::io::Error::other("file vanished after writing"),
    })?;
    state.local.insert(
        write.path.clone(),
        LocalFile {
            entry: write.entry,
            content: write.content,
            size: disk.size,
            mtime_ns: disk.mtime_ns,
            executable: disk.executable,
        },
    );
    if let Some(old) = &write.old_path {
        if let Some(local) = state.local.remove(old) {
            let abs = root.join(old);
            if unchanged(&abs, &local) {
                let _ = fs::remove_file(&abs);
            }
        }
    }
    Ok(true)
}

fn assemble(inner: &Inner, manifest: &Manifest, tmp: &Path, write: &PendingWrite) -> Result<()> {
    let mut file = fs::File::create(tmp).map_err(io_err(tmp))?;
    let mut hasher = blake3::Hasher::new();
    for chunk in &manifest.chunks {
        let data = inner
            .store
            .lock()
            .unwrap()
            .get(&chunk.hash)?
            .ok_or_else(|| Error::Io {
                path: tmp.to_path_buf(),
                source: std::io::Error::other("chunk evicted during assembly"),
            })?;
        hasher.update(&data);
        file.write_all(&data).map_err(io_err(tmp))?;
    }
    if hasher.finalize().as_bytes() != manifest.content.as_bytes() || manifest.len() != write.size {
        return Err(Error::Io {
            path: tmp.to_path_buf(),
            source: std::io::Error::other("assembled content does not match its hash"),
        });
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
    Ok(())
}
