//! The recycle bin: listing, emptying and restoring deleted files, and purging expired ones.

use std::collections::BTreeSet;
use std::sync::Arc;

use tidefell_proto::sync::EntryKind;
use tidefell_proto::{EntryId, FolderId};
use tracing::{info, warn};

use super::{Engine, FolderHandle, Inner};
use crate::state::FolderState;
use crate::views::{Event, RecycleSummary, RecycledInfo};
use crate::{now_ms, setaside, Error, Result};

impl Engine {
    /// Purges recycle-bin items that expired at or before `now_ms`, and their unused chunks.
    pub fn purge(&self, now_ms: i64) {
        purge(&self.inner, now_ms);
    }

    /// Deleted files in `folder`'s recycle bin, and files removed from this device only (ADR
    /// 0025), most recently deleted first.
    pub fn recycle_bin(&self, folder: FolderId) -> Result<Vec<RecycledInfo>> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let bin = state.meta.recycle_bin();
        let mut items: Vec<RecycledInfo> = bin
            .iter()
            .map(|r| recycled_info(r, bin.expires_at_ms(r)))
            .chain(setaside::infos(&state))
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
        let (discarded, set_aside, chunks) = {
            let mut state = handle.lock();
            if let Some(entry) = &entry {
                if state.meta.recycle_bin().get(entry).is_none()
                    && !state.set_aside.contains_key(entry)
                {
                    return Err(Error::NotRecycled(*entry));
                }
            }
            let set_aside = setaside::discard(&mut state, entry.as_ref());
            let (discarded, chunks) = state.discard(entry.as_ref());
            state.save()?;
            (discarded, set_aside, chunks)
        };
        info!(
            folder = %folder,
            items = discarded.len(),
            chunks = chunks.len(),
            "emptied recycle bin"
        );
        remove_chunks(&self.inner, chunks);
        if !discarded.is_empty() || !set_aside.is_empty() {
            self.inner.emit(Event::FolderChanged(folder));
        }
        Ok(discarded
            .iter()
            .map(|r| recycled_info(r, None))
            .chain(set_aside.iter().map(|s| set_aside_info(s, None)))
            .collect())
    }

    /// Restores an entry from the recycle bin. A file deleted everywhere reappears on every
    /// replica, including this one once the materialiser runs. A file removed from this device
    /// only is moved back into the folder here (ADR 0025).
    pub async fn restore(&self, folder: FolderId, entry: EntryId) -> Result<()> {
        let handle = self.inner.folder(&folder)?;
        let here_only = {
            let state = handle.lock();
            state.set_aside.contains_key(&entry) && state.meta.recycle_bin().get(&entry).is_none()
        };
        if here_only {
            let _guard = handle.fs_lock.lock().await;
            let path = {
                let mut state = handle.lock();
                let path = setaside::restore(&mut state, &entry)?;
                state.save()?;
                path
            };
            info!(folder = %folder, path, "restored on this device");
            self.inner.emit(Event::FileUpdated { folder, path });
            self.inner.emit(Event::FolderChanged(folder));
            return Ok(());
        }
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
        let set_aside = setaside::purge(&mut state, now_ms);
        let (purged, chunks) = state.purge(now_ms);
        if !purged.is_empty() || !chunks.is_empty() || !set_aside.is_empty() {
            info!(
                folder = %id,
                items = purged.len() + set_aside.len(),
                chunks = chunks.len(),
                "purged recycle bin"
            );
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
        here_only: false,
    }
}

fn set_aside_info(item: &crate::state::SetAside, expires_at_ms: Option<i64>) -> RecycledInfo {
    RecycledInfo {
        entry: item.local.entry,
        path: item.path.clone(),
        size: item.local.size,
        deleted_at_ms: item.removed_at_ms,
        expires_at_ms,
        here_only: true,
    }
}

pub(super) fn recycle_summary(state: &FolderState) -> RecycleSummary {
    let bin = state.meta.recycle_bin();
    let set_aside_expiry = state
        .set_aside
        .values()
        .filter_map(|item| setaside::expires_at_ms(state, item))
        .min();
    RecycleSummary {
        items: bin.len() + state.set_aside.len(),
        bytes: bin.iter().map(recycled_size).sum::<u64>()
            + state.set_aside.values().map(|s| s.local.size).sum::<u64>(),
        next_expiry_ms: match (bin.next_expiry_ms(), set_aside_expiry) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        },
    }
}

impl Engine {
    /// Removes the file at `path` from this device only (ADR 0025): it moves to this device's
    /// recycle bin, stays on every other device, and is not downloaded again until opened or
    /// restored.
    ///
    /// Callers decide when this is safe; the Android app only removes files a full replica has
    /// confirmed holding (ADR 0022).
    pub async fn remove_here(&self, folder: FolderId, path: &str) -> Result<()> {
        let handle = self.inner.folder(&folder)?;
        {
            let _guard = handle.fs_lock.lock().await;
            let mut state = handle.lock();
            setaside::remove_here(&mut state, path, now_ms())?;
            state.save()?;
        }
        info!(folder = %folder, path, "removed from this device");
        self.inner.emit(Event::FileRemoved {
            folder,
            path: path.to_owned(),
        });
        self.inner.emit(Event::FolderChanged(folder));
        Ok(())
    }
}
