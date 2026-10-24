//! Keeping partial replicas within their budget (ADR 0016).

use std::fs;
use std::sync::Arc;

use shoal_proto::sync::EntryKind;
use shoal_proto::FolderId;
use tracing::{debug, info};

use crate::config::ReplicaMode;
use crate::engine::{FolderHandle, Inner};
use crate::pins::is_pinned;
use crate::scan::unchanged;
use crate::views::Event;
use crate::Result;

/// A materialised file that may be evicted.
struct Candidate {
    path: String,
    size: u64,
    content: shoal_proto::ContentHash,
    /// True if this device made the current version, so another copy must be confirmed first.
    authored_here: bool,
}

/// True if a full replica that counts has confirmed holding `content` (ADR 0022).
fn has_confirmed_copy(
    inner: &Inner,
    handle: &FolderHandle,
    content: &shoal_proto::ContentHash,
) -> bool {
    let state = handle.lock();
    let vouching = inner.vouching.lock().unwrap();
    !crate::copies::confirmed(&state, &vouching, content).is_empty()
}

/// Evicts least recently used files of partial folder `id` until its materialised files fit
/// its budget. Pinned files (ADR 0029) are never evicted and do not count against the budget;
/// files held open are not evicted either. Callers hold the folder's `fs_lock`.
pub(crate) fn evict(inner: &Arc<Inner>, id: FolderId, handle: &FolderHandle) -> Result<()> {
    let (root, budget, mut usage, candidates) = {
        let state = handle.lock();
        let ReplicaMode::Partial { budget_bytes } = state.config.mode else {
            return Ok(());
        };
        let usage: u64 = state
            .local
            .iter()
            .filter(|(path, _)| !is_pinned(&state.pins, path))
            .map(|(_, l)| l.size)
            .sum();
        if usage <= budget_bytes {
            return Ok(());
        }
        let open = handle.open_paths();
        let mut candidates: Vec<(i64, Candidate)> = state
            .local
            .iter()
            .filter(|(path, _)| !open.contains(&***path) && !is_pinned(&state.pins, path))
            .map(|(path, local)| {
                let authored_here = state.meta.current(&local.entry).is_none_or(|v| {
                    v.author() == inner.key.id()
                        || !matches!(v.kind, EntryKind::File { content, .. } if *content == local.content)
                });
                let used = state.used.get(&**path).copied().unwrap_or(0);
                (
                    used,
                    Candidate {
                        path: path.to_string(),
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

    let mut evicted = false;
    for candidate in candidates {
        if usage <= budget {
            break;
        }
        if candidate.authored_here && !has_confirmed_copy(inner, handle, &candidate.content) {
            debug!(folder = %id, path = %candidate.path, "keeping: no full replica holds it yet");
            continue;
        }
        let abs = root.join(&candidate.path);
        let still = |state: &crate::state::FolderState| {
            let local = state.local.get(&candidate.path).map(|l| l.to_file())?;
            (local.content == candidate.content
                && !handle.open_paths().contains(&candidate.path)
                && !is_pinned(&state.pins, &candidate.path)
                // Edited since the last scan otherwise; the next scan records it.
                && unchanged(&abs, local.size, local.mtime_ns, 0))
            .then_some(local.entry)
        };
        let Some(entry) = still(&handle.lock()) else {
            continue;
        };
        // Freed in place, outside the state's lock: Windows may take its time.
        let freer = inner.freers.lock().unwrap().get(&id).cloned();
        let freed = freer.is_some_and(|free| {
            free(&abs, entry, candidate.content).unwrap_or_else(|e| {
                debug!(folder = %id, path = %candidate.path, error = %e, "cannot free in place");
                false
            })
        });
        let mut state = handle.lock();
        if freed {
            state.freed(&candidate.path);
            info!(folder = %id, path = %candidate.path, "evicted, freed in place");
        } else {
            if still(&state).is_none() {
                continue;
            }
            let _ = fs::remove_file(&abs);
            state.local.remove(&candidate.path);
            state.used.remove(&candidate.path);
            info!(folder = %id, path = %candidate.path, "evicted");
        }
        usage = usage.saturating_sub(candidate.size);
        evicted = true;
        drop(state);
        inner.emit(Event::FileRemoved {
            folder: id,
            path: candidate.path.clone(),
        });
    }
    if evicted {
        handle.lock().save()?;
    }
    Ok(())
}
