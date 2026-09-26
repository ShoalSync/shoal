//! The recycle bin: listing, emptying and restoring deleted files, and purging expired ones.

use std::collections::BTreeSet;
use std::sync::Arc;

use tidefell_proto::sync::EntryKind;
use tidefell_proto::{EntryId, FolderId};
use tracing::{info, warn};

use super::{Engine, FolderHandle, Inner};
use crate::state::FolderState;
use crate::views::{Event, RecycleSummary, RecycledInfo};
use crate::{now_ms, Error, Result};

impl Engine {
    /// Purges recycle-bin items that expired at or before `now_ms`, and their unused chunks.
    pub fn purge(&self, now_ms: i64) {
        purge(&self.inner, now_ms);
    }

    /// Deleted files in `folder`'s recycle bin, most recently deleted first.
    pub fn recycle_bin(&self, folder: FolderId) -> Result<Vec<RecycledInfo>> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let bin = state.meta.recycle_bin();
        let mut items: Vec<RecycledInfo> = bin
            .iter()
            .map(|r| recycled_info(r, bin.expires_at_ms(r)))
            .collect();
        items.sort_by_key(|i| std::cmp::Reverse(i.deleted_at_ms));
        Ok(items)
    }

    /// Permanently removes `entry`, or every item if `None`, from this replica's recycle bin
    /// and frees its content. Other replicas keep their copies. Returns the removed items.
    pub fn empty_recycle_bin(
        &self,
        folder: FolderId,
        entry: Option<EntryId>,
    ) -> Result<Vec<RecycledInfo>> {
        let handle = self.inner.folder(&folder)?;
        let (discarded, chunks) = {
            let mut state = handle.lock();
            if let Some(entry) = &entry {
                if state.meta.recycle_bin().get(entry).is_none() {
                    return Err(Error::NotRecycled(*entry));
                }
            }
            let (discarded, chunks) = state.discard(entry.as_ref());
            state.save()?;
            (discarded, chunks)
        };
        info!(
            folder = %folder,
            items = discarded.len(),
            chunks = chunks.len(),
            "emptied recycle bin"
        );
        remove_chunks(&self.inner, chunks);
        if !discarded.is_empty() {
            self.inner.emit(Event::FolderChanged(folder));
        }
        Ok(discarded.iter().map(|r| recycled_info(r, None)).collect())
    }

    /// Restores a deleted entry from the recycle bin. The file reappears on every replica,
    /// including this one once the materialiser runs.
    pub fn restore(&self, folder: FolderId, entry: EntryId) -> Result<()> {
        let handle = self.inner.folder(&folder)?;
        {
            let mut state = handle.lock();
            state
                .meta
                .restore(self.inner.key.signing_key(), entry, now_ms())?;
            state.save()?;
        }
        self.inner.emit(Event::FolderChanged(folder));
        self.inner.changed();
        Ok(())
    }
}

pub(super) fn purge(inner: &Inner, now_ms: i64) {
    let handles: Vec<(FolderId, Arc<FolderHandle>)> = inner
        .folders
        .read()
        .unwrap()
        .iter()
        .map(|(id, h)| (*id, h.clone()))
        .collect();
    let mut orphaned = BTreeSet::new();
    for (id, handle) in &handles {
        let mut state = handle.lock();
        let (purged, chunks) = state.purge(now_ms);
        if !purged.is_empty() || !chunks.is_empty() {
            info!(folder = %id, items = purged.len(), chunks = chunks.len(), "purged recycle bin");
            if let Err(e) = state.save() {
                warn!(folder = %id, error = %e, "saving after purge failed");
            }
        }
        orphaned.extend(chunks);
    }
    remove_chunks(inner, orphaned);
}

/// Removes chunks one folder no longer references from the block store, unless another
/// folder still uses them.
fn remove_chunks(inner: &Inner, chunks: impl IntoIterator<Item = tidefell_proto::ChunkHash>) {
    let handles: Vec<Arc<FolderHandle>> = inner.folders.read().unwrap().values().cloned().collect();
    let still_used = |hash: &tidefell_proto::ChunkHash| {
        handles.iter().any(|h| h.lock().index.is_referenced(hash))
    };
    let mut store = inner.store.lock().unwrap();
    for hash in chunks.into_iter().collect::<BTreeSet<_>>() {
        if !still_used(&hash) {
            let _ = store.remove(&hash);
        }
    }
}

fn recycled_size(item: &tidefell_meta::Recycled) -> u64 {
    match item.version.kind {
        EntryKind::File { size, .. } => size,
        _ => 0,
    }
}

fn recycled_info(item: &tidefell_meta::Recycled, expires_at_ms: Option<i64>) -> RecycledInfo {
    RecycledInfo {
        entry: item.version.entry,
        path: item.version.path.clone(),
        size: recycled_size(item),
        deleted_at_ms: item.deleted_at_ms,
        expires_at_ms,
    }
}

pub(super) fn recycle_summary(state: &FolderState) -> RecycleSummary {
    let bin = state.meta.recycle_bin();
    RecycleSummary {
        items: bin.len(),
        bytes: bin.iter().map(recycled_size).sum(),
        next_expiry_ms: bin.next_expiry_ms(),
    }
}
