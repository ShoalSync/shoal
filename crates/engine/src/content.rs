//! Where file content is kept on this device (ADR 0027).
//!
//! Content that is on disk, as a file in the folder or as a deleted file kept in the folder's
//! reserved directory, is read from there: chunks are located through the folder's manifests and
//! checked against their hashes, so a file changed since the last scan is never served. The block
//! store holds only content that is on neither: chunks being downloaded, recycle-bin items and
//! conflict copies without a file, and the pending versions of a replica that will write them.

use std::collections::HashSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use tidefell_chunker::hash_chunk;
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ChunkHash, ContentHash, EntryId, Manifest};
use tracing::{debug, info};

use crate::config::{Direction, ReplicaMode};
use crate::engine::{FolderHandle, Inner};
use crate::scan::RESERVED_DIR;
use crate::state::FolderState;
use crate::{io_err, random_bytes, Result};

/// Where deleted files are kept for the recycle bin, inside the folder's reserved directory.
pub(crate) fn kept_dir(root: &Path) -> PathBuf {
    root.join(RESERVED_DIR).join("deleted")
}

/// A place on disk to read one chunk from.
struct Source {
    path: PathBuf,
    offset: u64,
    len: u32,
}

/// Where chunk `hash` starts in `manifest`, and its length.
fn position(manifest: &Manifest, hash: &ChunkHash) -> Option<(u64, u32)> {
    let mut offset = 0u64;
    for chunk in &manifest.chunks {
        if chunk.hash == *hash {
            return Some((offset, chunk.len));
        }
        offset += u64::from(chunk.len);
    }
    None
}

/// Every file on disk that should contain chunk `hash`, by the folder's records.
fn sources(state: &FolderState, hash: &ChunkHash) -> Vec<Source> {
    let root = &state.config.path;
    let mut out = Vec::new();
    for content in &state.manifests.contents_with(hash) {
        let Some((offset, len)) = state
            .manifests
            .get(content)
            .and_then(|m| position(&m, hash))
        else {
            continue;
        };
        for path in state.local.paths_with(content) {
            out.push(Source {
                path: root.join(path),
                offset,
                len,
            });
        }
        if let Some(name) = state.kept.get(content) {
            out.push(Source {
                path: kept_dir(root).join(name),
                offset,
                len,
            });
        }
        // Previous versions are read for restoring them here, but never count as held for
        // other devices (see `has_chunk`): history is thinned and would lose a confirmed copy.
        for version in state.history.values().flatten() {
            if version.content == *content {
                out.push(Source {
                    path: crate::history::history_dir(root).join(&version.file),
                    offset,
                    len,
                });
            }
        }
    }
    out
}

/// Reads a chunk from `source` if it still hashes to `hash`.
fn read_verified(source: &Source, hash: &ChunkHash) -> Option<Vec<u8>> {
    let mut file = fs::File::open(&source.path).ok()?;
    file.seek(SeekFrom::Start(source.offset)).ok()?;
    let mut data = vec![0; source.len as usize];
    file.read_exact(&mut data).ok()?;
    if hash_chunk(&data) != *hash {
        debug!(path = %source.path.display(), "file changed since it was scanned");
        return None;
    }
    Some(data)
}

/// Chunk `hash` of the folder, from the block store or from a file on disk that contains it.
pub(crate) fn read_chunk(
    inner: &Inner,
    handle: &FolderHandle,
    hash: &ChunkHash,
) -> Option<Vec<u8>> {
    if let Ok(Some(data)) = inner.store.lock().unwrap().get(hash) {
        return Some(data);
    }
    // Read files without holding the folder's lock.
    let sources = sources(&handle.lock(), hash);
    let found = sources
        .iter()
        .find_map(|source| read_verified(source, hash));
    if found.is_none() && !sources.is_empty() {
        // The records say a file holds it and none does, as they can until the next scan: it
        // is not counted as here until it is fetched.
        inner.unreadable.lock().unwrap().insert(*hash);
    }
    found
}

/// True if chunk `hash` is in the block store or, by the folder's records, in a file on disk.
/// Files are not read, so a file changed since the last scan still counts until a read fails;
/// then it does not until it is fetched.
pub(crate) fn has_chunk(inner: &Inner, state: &FolderState, hash: &ChunkHash) -> bool {
    inner.store.lock().unwrap().contains(hash)
        || !inner.unreadable.lock().unwrap().contains(hash)
            && state
                .manifests
                .contents_with(hash)
                .iter()
                .any(|c| state.local.paths_with(c).next().is_some() || state.kept.contains_key(c))
}

/// Takes the file at `abs`, unchanged since it was recorded with `content`, off the folder
/// without losing its content: it is moved into the reserved directory for the recycle bin,
/// unless the content is already on disk elsewhere, in which case it is deleted. If `new` is
/// given, it takes the file's place with no moment where the path is missing. The caller has
/// already forgotten it in `state.local`.
pub(crate) fn keep_deleted(
    state: &mut FolderState,
    abs: &Path,
    content: ContentHash,
    entry: EntryId,
    new: Option<&Path>,
) -> Result<()> {
    let elsewhere =
        state.local.paths_with(&content).next().is_some() || state.kept.contains_key(&content);
    if elsewhere {
        return match new {
            Some(new) => crate::rename_durably(new, abs).map_err(io_err(abs)),
            None => fs::remove_file(abs).map_err(io_err(abs)),
        };
    }
    let dir = kept_dir(&state.config.path);
    fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    // Keeps Android's media scanner out of the engine's own files.
    let nomedia = state.config.path.join(RESERVED_DIR).join(".nomedia");
    if !nomedia.exists() {
        fs::write(&nomedia, b"").map_err(io_err(&nomedia))?;
    }
    // The entry it was a version of is in the name, for if the entry is live again by the time
    // the bin no longer needs it (see [`release_kept`]).
    let name = format!(
        "{content}-{entry}-{:016x}",
        u64::from_le_bytes(random_bytes()?)
    );
    match new {
        Some(new) => crate::history::swap_in(&state.config.path, new, abs, &dir.join(&name))
            .map_err(io_err(abs))?,
        None => crate::rename_durably(abs, dir.join(&name)).map_err(io_err(abs))?,
    }
    crate::sidecar::write(&dir.join(&name), &crate::sidecar::kept(state, content));
    state.kept.insert(content, name);
    Ok(())
}

/// Stops keeping `content`'s kept file. A deleted version the recycle bin no longer holds,
/// because an edit made apart from the delete brought its entry back, perhaps to be deleted
/// again, becomes a previous version of that entry, as it would have had the edit come first
/// (ADR 0029), unless its content is in the folder anyway; anything else is deleted.
pub(crate) fn release_kept(state: &mut FolderState, content: ContentHash) {
    let Some(name) = state.kept.remove(&content) else {
        return;
    };
    let file = kept_dir(&state.config.path).join(&name);
    let revived = state
        .local
        .paths_with(&content)
        .next()
        .is_none()
        .then(|| kept_entry(&name))
        .flatten()
        .and_then(|entry| {
            let current = state.meta.current(&entry)?;
            Some((entry, current.path.to_owned()))
        });
    crate::sidecar::remove(&file);
    let adopted = revived.is_some_and(|(entry, path)| {
        crate::history::adopt(state, entry, &path, &file, content)
            .map_err(|e| tracing::warn!(error = %e, "keeping a deleted version failed"))
            .is_ok()
    });
    if !adopted {
        let _ = fs::remove_file(&file);
    }
}

/// The entry a kept file's name says it was a version of. Files kept by earlier versions do not
/// say.
fn kept_entry(name: &str) -> Option<EntryId> {
    let mut parts = name.split('-');
    let (_content, entry, _random) = (parts.next()?, parts.next()?, parts.next()?);
    entry.parse().ok()
}

/// Deletes kept files that are no longer needed: their content is back in a file in the folder,
/// or nothing references it any more.
fn tidy_kept(state: &mut FolderState) {
    tidy_unneeded(state);
    // After the above, which reads what the sidecars say of items gone from the bin since.
    state.refresh_sidecars();
}

fn tidy_unneeded(state: &mut FolderState) {
    if state.kept.is_empty() {
        return;
    }
    let mut referenced = HashSet::new();
    let mut add = |kind: &EntryKind| {
        if let EntryKind::File { content, .. } = kind {
            referenced.insert(*content);
        }
    };
    for version in state.meta.entries() {
        add(version.kind);
        for conflict in state.meta.conflicts(&version.entry) {
            add(&conflict.version.kind);
        }
    }
    for item in state.meta.recycle_bin().iter() {
        add(&item.version.kind);
    }
    let unneeded: Vec<ContentHash> = state
        .kept
        .keys()
        .filter(|c| !referenced.contains(*c) || state.local.paths_with(c).next().is_some())
        .copied()
        .collect();
    if unneeded.is_empty() {
        return;
    }
    for content in unneeded {
        release_kept(state, content);
    }
    if let Err(e) = state.save() {
        tracing::warn!(error = %e, "saving after tidying kept files failed");
    }
}

/// Contents this folder references that are on disk nowhere, so their chunks belong in the
/// block store: recycle-bin items and conflict copies, and live versions this replica is going
/// to write. Placeholders, files dropped from this device and every file of a send-only replica
/// are not written, so their chunks are not kept.
fn store_only(state: &FolderState) -> HashSet<ContentHash> {
    let on_disk =
        |c: &ContentHash| state.local.paths_with(c).next().is_some() || state.kept.contains_key(c);
    let partial = matches!(state.config.mode, ReplicaMode::Partial { .. });
    let send_only = state.config.sync.direction == Direction::SendOnly;
    let materialised: HashSet<EntryId> = state.local.values().map(|l| l.entry).collect();
    let mut out = HashSet::new();
    let mut add = |kind: &EntryKind| {
        if let EntryKind::File { content, .. } = kind {
            if !on_disk(content) {
                out.insert(*content);
            }
        }
    };
    for version in state.meta.entries() {
        let path = state.meta.disk_path(&version.entry).unwrap_or(version.path);
        let writes = !send_only
            && !state.dropped.contains(&version.entry)
            && !state.ignores.is_ignored(path, false)
            && (!partial || materialised.contains(&version.entry));
        if writes {
            add(version.kind);
        }
        for conflict in state.meta.conflicts(&version.entry) {
            add(&conflict.version.kind);
        }
    }
    for item in state.meta.recycle_bin().iter() {
        add(&item.version.kind);
    }
    out
}

/// Removes from the block store every chunk nothing needs there: chunks of content on disk, and
/// chunks no content references. Chunks of content being downloaded are kept. Returns the
/// number of chunks removed.
pub(crate) fn compact(inner: &Inner) -> usize {
    let mut needed: HashSet<ChunkHash> = HashSet::new();
    for handle in inner.handles() {
        let mut state = handle.lock();
        tidy_kept(&mut state);
        let fetching = inner.fetching.lock().unwrap().clone();
        for content in store_only(&state).iter().chain(fetching.iter()) {
            if let Some(manifest) = state.manifests.get(content) {
                needed.extend(manifest.chunks.iter().map(|c| c.hash));
            }
        }
    }
    let mut store = inner.store.lock().unwrap();
    let unneeded: Vec<ChunkHash> = store
        .hashes()
        .filter(|h| !needed.contains(h))
        .copied()
        .collect();
    for hash in &unneeded {
        let _ = store.remove(hash);
    }
    if !unneeded.is_empty() {
        info!(
            chunks = unneeded.len(),
            "released chunks the block store does not need"
        );
    }
    unneeded.len()
}

#[cfg(test)]
mod tests {
    use tidefell_proto::ChunkRef;

    use super::*;

    #[test]
    fn positions_follow_chunk_lengths() {
        let manifest = Manifest {
            content: ContentHash([1; 32]),
            chunks: vec![
                ChunkRef {
                    hash: ChunkHash([2; 32]),
                    len: 10,
                },
                ChunkRef {
                    hash: ChunkHash([3; 32]),
                    len: 20,
                },
                ChunkRef {
                    hash: ChunkHash([4; 32]),
                    len: 5,
                },
            ],
        };
        assert_eq!(position(&manifest, &ChunkHash([2; 32])), Some((0, 10)));
        assert_eq!(position(&manifest, &ChunkHash([4; 32])), Some((30, 5)));
        assert_eq!(position(&manifest, &ChunkHash([9; 32])), None);
    }
}
