//! Keeping partial replicas within their budget (ADR 0016).

use std::collections::HashSet;
use std::fs;
use std::sync::Arc;

use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ChunkHash, FolderId};
use tracing::{debug, info};

use crate::config::ReplicaMode;
use crate::engine::{FolderHandle, Inner};
use crate::scan::unchanged;
use crate::views::Event;
use crate::Result;

/// A materialised file that may be evicted.
struct Candidate {
    path: String,
    size: u64,
    content: tidefell_proto::ContentHash,
    /// True if this device made the current version, so another copy must be confirmed first.
    authored_here: bool,
}

/// True if a full replica has confirmed holding `content` (ADR 0022).
fn has_confirmed_copy(handle: &FolderHandle, content: &tidefell_proto::ContentHash) -> bool {
    handle
        .lock()
        .copies
        .get(content)
        .is_some_and(|devices| !devices.is_empty())
}

/// Evicts least recently used, unpinned files of partial folder `id` until its materialised
/// files fit its budget. Callers hold the folder's `fs_lock`.
pub(crate) fn evict(inner: &Arc<Inner>, id: FolderId, handle: &FolderHandle) -> Result<()> {
    let (root, budget, mut usage, candidates) = {
        let state = handle.lock();
        let ReplicaMode::Partial { budget_bytes } = state.config.mode else {
            return Ok(());
        };
        let usage: u64 = state.local.values().map(|l| l.size).sum();
        if usage <= budget_bytes {
            return Ok(());
        }
        let pinned = handle.pinned();
        let mut candidates: Vec<(i64, Candidate)> = state
            .local
            .iter()
            .filter(|(path, _)| !pinned.contains(*path))
            .map(|(path, local)| {
                let authored_here = state.meta.current(&local.entry).is_none_or(|v| {
                    v.author == inner.key.id()
                        || !matches!(v.kind, EntryKind::File { content, .. } if content == local.content)
                });
                let used = state.used.get(path).copied().unwrap_or(0);
                (
                    used,
                    Candidate {
                        path: path.clone(),
                        size: local.size,
                        content: local.content,
                        authored_here,
                    },
                )
            })
            .collect();
        candidates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.path.cmp(&b.1.path)));
        (
            state.config.path.clone(),
            budget_bytes,
            usage,
            candidates.into_iter().map(|(_, c)| c).collect::<Vec<_>>(),
        )
    };

    let mut released = Vec::new();
    for candidate in candidates {
        if usage <= budget {
            break;
        }
        if candidate.authored_here && !has_confirmed_copy(handle, &candidate.content) {
            debug!(folder = %id, path = %candidate.path, "keeping: no full replica holds it yet");
            continue;
        }
        let mut state = handle.lock();
        let Some(local) = state.local.get(&candidate.path).cloned() else {
            continue;
        };
        if local.content != candidate.content || handle.pinned().contains(&candidate.path) {
            continue;
        }
        let abs = root.join(&candidate.path);
        if !unchanged(&abs, &local) {
            // Edited since the last scan; the next scan records it.
            continue;
        }
        let _ = fs::remove_file(&abs);
        state.local.remove(&candidate.path);
        state.used.remove(&candidate.path);
        usage = usage.saturating_sub(candidate.size);
        released.push(candidate.content);
        info!(folder = %id, path = %candidate.path, "evicted");
        drop(state);
        inner.emit(Event::FileRemoved {
            folder: id,
            path: candidate.path.clone(),
        });
    }
    if released.is_empty() {
        return Ok(());
    }
    handle.lock().save()?;
    drop_unneeded_chunks(inner, handle, &released);
    Ok(())
}

/// Removes the chunks of `contents` from the block store unless a replica that holds everything
/// references them, or another folder has a file on disk that uses them.
pub(crate) fn drop_unneeded_chunks(
    inner: &Inner,
    handle: &FolderHandle,
    contents: &[tidefell_proto::ContentHash],
) {
    let chunks: HashSet<ChunkHash> = {
        let state = handle.lock();
        contents
            .iter()
            .filter_map(|c| state.manifests.get(c))
            .flat_map(|m| m.chunks.iter().map(|c| c.hash))
            .collect()
    };
    let mut needed = HashSet::new();
    for other in inner.handles() {
        let state = other.lock();
        if state.config.holds_everything() {
            needed.extend(
                chunks
                    .iter()
                    .filter(|h| state.index.is_referenced(h))
                    .copied(),
            );
        } else {
            for local in state.local.values() {
                if let Some(manifest) = state.manifests.get(&local.content) {
                    needed.extend(manifest.chunks.iter().map(|c| c.hash));
                }
            }
        }
    }
    let mut store = inner.store.lock().unwrap();
    for hash in chunks.difference(&needed) {
        let _ = store.remove(hash);
    }
}
