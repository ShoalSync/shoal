//! What a storage peer keeps of a stored folder's content (ADR 0023, "On storage peers"):
//! sealed chunks by chunk ID, as fetched, which it can neither open nor check. Members check
//! every chunk they fetch from it by its AEAD tag.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use shoal_proto::sync2::{ChunkList, Item, SealedVersion};
use shoal_proto::{ChunkId, DeviceId, SignedKind as _};
use tracing::{debug, info, warn};

use crate::engine::{FolderHandle, Inner};
use crate::state::FolderState;
use crate::{io_err, random_bytes, Result};

/// A stored folder's sealed chunks: one file each under `<path>/chunks`, named by chunk ID.
#[derive(Debug)]
pub(crate) struct SealedStore {
    dir: PathBuf,
    ids: BTreeSet<ChunkId>,
    bytes: u64,
}

impl SealedStore {
    /// Opens the store under `path`, counting what it holds.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let dir = path.join("chunks");
        fs::create_dir_all(&dir).map_err(io_err(&dir))?;
        let mut store = Self {
            dir,
            ids: BTreeSet::new(),
            bytes: 0,
        };
        for prefix in fs::read_dir(&store.dir)
            .map_err(io_err(&store.dir))?
            .flatten()
        {
            let Ok(files) = fs::read_dir(prefix.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name();
                let Some(id) = name.to_str().and_then(|n| n.parse::<ChunkId>().ok()) else {
                    // A write that did not finish.
                    let _ = fs::remove_file(file.path());
                    continue;
                };
                store.bytes = store
                    .bytes
                    .saturating_add(file.metadata().map_or(0, |m| m.len()));
                store.ids.insert(id);
            }
        }
        Ok(store)
    }

    fn path(&self, id: &ChunkId) -> PathBuf {
        let hex = id.to_string();
        self.dir.join(&hex[..2]).join(hex)
    }

    pub(crate) fn has(&self, id: &ChunkId) -> bool {
        self.ids.contains(id)
    }

    /// The sealed bytes of `id`, as stored.
    pub(crate) fn get(&self, id: &ChunkId) -> Option<Vec<u8>> {
        self.has(id).then(|| fs::read(self.path(id)).ok()).flatten()
    }

    /// Keeps `sealed` as chunk `id`.
    pub(crate) fn put(&mut self, id: ChunkId, sealed: &[u8]) -> Result<()> {
        if self.has(&id) {
            return Ok(());
        }
        let path = self.path(&id);
        let parent = path.parent().unwrap_or(&self.dir).to_owned();
        fs::create_dir_all(&parent).map_err(io_err(&parent))?;
        let tmp = parent.join(format!(".{:016x}", u64::from_le_bytes(random_bytes()?)));
        fs::write(&tmp, sealed).map_err(io_err(&tmp))?;
        crate::rename_durably(&tmp, &path).map_err(io_err(&path))?;
        self.ids.insert(id);
        self.bytes = self.bytes.saturating_add(sealed.len() as u64);
        Ok(())
    }

    /// Removes the chunks not in `keep`. Returns how many it removed.
    fn retain(&mut self, keep: &BTreeSet<ChunkId>) -> usize {
        let gone: Vec<ChunkId> = self.ids.difference(keep).copied().collect();
        for id in &gone {
            let path = self.path(id);
            let len = fs::metadata(&path).map_or(0, |m| m.len());
            if fs::remove_file(&path).is_ok() {
                self.ids.remove(id);
                self.bytes = self.bytes.saturating_sub(len);
            }
        }
        gone.len()
    }

    /// The sealed bytes it holds.
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// The chunk IDs the kept sealed versions list, the current ones first, each with the highest
/// priority members gave the entries listing it, highest first within each (ADR 0023's
/// amendment of 2026-10-22): what the folder's store must hold, in the order it fills. Worked out
/// once per change to the items, the list blobs held or the priorities, not on every pass and
/// status.
fn listed(state: &FolderState, current_only: bool) -> Vec<(ChunkId, u8)> {
    let at = (
        state.items_revision,
        state.list_blobs.len(),
        state.priorities_revision,
    );
    if let Some((revision, lists, priorities, all, current)) = state.stored_listed.borrow().as_ref()
    {
        if (*revision, *lists, *priorities) == at {
            return if current_only { current } else { all }.clone();
        }
    }
    let (all, current) = (listed_now(state, false), listed_now(state, true));
    let result = if current_only { &current } else { &all }.clone();
    *state.stored_listed.borrow_mut() = Some((at.0, at.1, at.2, all, current));
    result
}

/// The priority members gave `entry`, or the default.
fn entry_priority(state: &FolderState, entry: &shoal_proto::EntryId) -> u8 {
    state
        .storage_priorities
        .get(entry)
        .map_or(shoal_proto::storage::DEFAULT_PRIORITY, |p| p.priority)
}

fn listed_now(state: &FolderState, current_only: bool) -> Vec<(ChunkId, u8)> {
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    for pass in [false, true] {
        if pass && current_only {
            break;
        }
        let mut versions: Vec<(u8, u64, Vec<ChunkId>)> = Vec::new();
        for (sequence, item) in state.items.iter() {
            let Item::Version(signed) = item else {
                continue;
            };
            if state.superseded.contains_key(sequence) != pass {
                continue;
            }
            let Ok(sealed) = SealedVersion::decode(&signed.body) else {
                continue;
            };
            // A version listing its chunks in list blobs counts once they are fetched.
            let Some(ids) = crate::encrypted::chunk_ids_of(state, &sealed) else {
                continue;
            };
            versions.push((entry_priority(state, &sealed.entry), *sequence, ids));
        }
        // Highest priority first; within one, in index order, as before.
        versions.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        for (priority, _, ids) in versions {
            for id in ids {
                if seen.insert(id) {
                    order.push((id, priority));
                }
            }
        }
    }
    order
}

/// Forgets what the store holds of entries ranked below `below`: the chunks no kept version of
/// an entry ranked at `below` or above lists, as a member asked for a user (ADR 0023's amendment
/// of 2026-10-22). Returns the chunks and sealed bytes removed. Never done otherwise.
pub(crate) fn drop_below(state: &mut FolderState, below: u8) -> (u64, u64) {
    let mut keep = BTreeSet::new();
    for item in state.items.values() {
        let Item::Version(signed) = item else {
            continue;
        };
        let Ok(sealed) = SealedVersion::decode(&signed.body) else {
            continue;
        };
        if entry_priority(state, &sealed.entry) < below {
            continue;
        }
        if let Some(ids) = crate::encrypted::chunk_ids_of(state, &sealed) {
            keep.extend(ids);
        }
    }
    let Some(store) = state.sealed_store.as_mut() else {
        return (0, 0);
    };
    let before = store.bytes();
    let removed = store.retain(&keep);
    (removed as u64, before.saturating_sub(store.bytes()))
}

/// Records the priorities member device `from` gave the folder's entries at `issued_ms`,
/// each replacing only one given earlier (`specs/storage-v1.md`).
pub(crate) fn set_priorities(
    state: &mut FolderState,
    from: DeviceId,
    issued_ms: i64,
    hints: &[shoal_proto::storage::Hint],
) -> bool {
    let mut changed = false;
    for hint in hints {
        if state
            .storage_priorities
            .get(&hint.entry)
            .is_some_and(|p| p.issued_ms >= issued_ms)
        {
            continue;
        }
        let was = state
            .storage_priorities
            .get(&hint.entry)
            .map(|p| p.priority);
        state.storage_priorities.insert(
            hint.entry,
            crate::state::StoredPriority {
                priority: hint.priority,
                issued_ms,
                from,
            },
        );
        changed |= was != Some(hint.priority);
    }
    if changed {
        state.priorities_revision += 1;
    }
    changed
}

/// How many chunks of the folder's current versions it lacks.
pub(crate) fn missing(state: &FolderState) -> usize {
    let Some(store) = state.sealed_store.as_ref() else {
        return 0;
    };
    // Not what members keep from it: it never fetches that.
    listed(state, true)
        .iter()
        .filter(|(id, priority)| *priority > 0 && !store.has(id))
        .count()
}

/// Whether the folder's store has reached its quota.
pub(crate) fn over_quota(state: &FolderState) -> bool {
    let quota = state
        .config
        .encryption
        .as_ref()
        .and_then(|e| e.stored)
        .map_or(u64::MAX, |s| s.quota_bytes);
    state
        .sealed_store
        .as_ref()
        .is_some_and(|store| store.bytes() >= quota)
}

/// The folder's priority against this device's other stored folders.
fn priority(state: &FolderState) -> u8 {
    state
        .config
        .encryption
        .as_ref()
        .and_then(|e| e.stored)
        .map_or(crate::config::Stored::DEFAULT_PRIORITY, |s| s.priority)
}

/// The peers sharing `folder` over sync/2.
fn peers_of(node: &shoal_net::Node, folder: shoal_proto::FolderId) -> Vec<DeviceId> {
    node.encrypted_peers()
        .into_iter()
        .filter(|p| p.shared_folders().contains(&folder))
        .map(|p| p.device())
        .collect()
}

/// Under a total quota, what the stored folders other than `folder` hold together, and
/// whether one of a higher priority than `ranked` still lacks chunks of its current versions
/// that a connected peer could send: it fills first (ADR 0023's amendment of 2026-10-22).
fn others(
    inner: &Inner,
    node: &shoal_net::Node,
    folder: shoal_proto::FolderId,
    ranked: u8,
) -> (u64, bool) {
    let mut held = 0u64;
    let mut higher_waiting = false;
    for handle in inner.handles() {
        let state = handle.lock();
        if state.config.id == folder || !state.config.is_stored() {
            continue;
        }
        held = held.saturating_add(state.sealed_store.as_ref().map_or(0, |s| s.bytes()));
        if priority(&state) > ranked
            && !over_quota(&state)
            && missing(&state) > 0
            && !peers_of(node, state.config.id).is_empty()
        {
            higher_waiting = true;
        }
    }
    (held, higher_waiting)
}

/// Fetches sealed chunks over blobs/2 for the scheduler, keeping them sealed: a storage peer
/// holds no key, so it can neither open nor check them (members do). Each chunk's ID stands in
/// for its hash, and a chunk's length is not known before it is fetched, so only the count of
/// chunks in flight bounds a request.
struct StoredSource {
    node: std::sync::Arc<shoal_net::Node>,
    folder: shoal_proto::FolderId,
}

impl shoal_fetch::ChunkSource for StoredSource {
    type Peer = DeviceId;

    async fn availability(
        &self,
        peer: &DeviceId,
        manifest: &shoal_proto::Manifest,
    ) -> std::result::Result<Vec<shoal_proto::ChunkHash>, String> {
        use shoal_proto::blobs2::{BlobId, MAX_BLOBS_PER_AVAILABILITY};
        let endpoint =
            shoal_net::EndpointId::from_bytes(peer.as_bytes()).map_err(|e| e.to_string())?;
        let mut held = Vec::new();
        for part in manifest.chunks.chunks(MAX_BLOBS_PER_AVAILABILITY) {
            let ids: Vec<BlobId> = part
                .iter()
                .map(|c| BlobId::from(ChunkId(c.hash.0)))
                .collect();
            let holds = self
                .node
                .blob_availability(endpoint, self.folder, &ids)
                .await
                .map_err(|e| e.to_string())?;
            held.extend(
                part.iter()
                    .zip(holds)
                    .filter(|(_, holds)| *holds)
                    .map(|(c, _)| c.hash),
            );
        }
        Ok(held)
    }

    async fn fetch(
        &self,
        peer: &DeviceId,
        chunks: &[shoal_proto::ChunkHash],
    ) -> std::result::Result<shoal_fetch::Fetched, String> {
        use shoal_net::{BlobKind, BlobResult};
        use shoal_proto::blobs2::BlobId;
        let endpoint =
            shoal_net::EndpointId::from_bytes(peer.as_bytes()).map_err(|e| e.to_string())?;
        let wanted: Vec<(BlobId, BlobKind)> = chunks
            .iter()
            .map(|h| (BlobId::from(ChunkId(h.0)), BlobKind::SealedChunk))
            .collect();
        let results = self
            .node
            .fetch_blobs(endpoint, self.folder, &wanted)
            .await
            .map_err(|e| e.to_string())?;
        let mut fetched = shoal_fetch::Fetched::default();
        for result in results {
            match result {
                // Only what was asked for: a peer cannot slip in another blob.
                BlobResult::Found(id, sealed) if chunks.iter().any(|h| h.0 == id.0) => {
                    fetched.chunks.push((shoal_proto::ChunkHash(id.0), sealed));
                }
                BlobResult::Busy(wait) => fetched.busy_for = Some(wait),
                _ => {}
            }
        }
        Ok(fetched)
    }
}

/// Why a stored folder's fetch stopped short.
const FULL: &str = "the quota is reached";

/// Fetches, from the peers sharing the folder, the chunks its kept versions list that it lacks,
/// the current versions' first, until it has them or its quota, or the total quota of all
/// stored folders, is reached. Under a total quota it waits while a folder of a higher priority
/// lacks what it could fetch.
pub(crate) async fn fetch(
    inner: &std::sync::Arc<Inner>,
    node: &std::sync::Arc<shoal_net::Node>,
    handle: &FolderHandle,
) -> Result<()> {
    let (folder, ranked, wanted) = {
        let state = handle.lock();
        let Some(store) = state.sealed_store.as_ref() else {
            return Ok(());
        };
        if over_quota(&state) {
            return Ok(());
        }
        let wanted: Vec<(ChunkId, u8)> = listed(&state, false)
            .into_iter()
            .filter(|(id, priority)| *priority > 0 && !store.has(id))
            .collect();
        (state.config.id, priority(&state), wanted)
    };
    if wanted.is_empty() {
        return Ok(());
    }
    let total = *inner.storage_quota.lock().unwrap();
    let mut others_hold = 0;
    if total.is_some() {
        let (held, higher_waiting) = others(inner, node, folder, ranked);
        if higher_waiting {
            debug!(%folder, "a stored folder of a higher priority fills first");
            return Ok(());
        }
        others_hold = held;
    }
    let peers = peers_of(node, folder);
    let source = std::sync::Arc::new(StoredSource {
        node: node.clone(),
        folder,
    });
    let mut fetched = 0usize;
    // A priority at a time, highest first: within one, chunks arrive in no set order, as
    // requests overlap, so a lower priority waits for a higher one to be done with.
    let mut tiers: Vec<u8> = wanted.iter().map(|(_, p)| *p).collect();
    tiers.sort_unstable_by(|a, b| b.cmp(a));
    tiers.dedup();
    for tier in tiers {
        let manifest = shoal_proto::Manifest {
            content: shoal_proto::ContentHash([0; 32]),
            chunks: wanted
                .iter()
                .filter(|(_, p)| *p == tier)
                .map(|(id, _)| shoal_proto::ChunkRef {
                    hash: shoal_proto::ChunkHash(id.0),
                    len: 0,
                })
                .collect(),
        };
        let result = shoal_fetch::fetch(
            source.clone(),
            &manifest,
            &peers,
            |_| false,
            shoal_fetch::Config::default(),
            |id, sealed| {
                let mut state = handle.lock();
                let full = over_quota(&state)
                    || total.is_some_and(|total| {
                        let here = state.sealed_store.as_ref().map_or(0, |s| s.bytes());
                        others_hold.saturating_add(here) >= total
                    });
                if full {
                    return Err(FULL.to_owned());
                }
                let Some(store) = state.sealed_store.as_mut() else {
                    return Err("the folder is no longer stored".to_owned());
                };
                store
                    .put(ChunkId(id.0), &sealed)
                    .map_err(|e| e.to_string())?;
                fetched += 1;
                Ok(())
            },
        )
        .await;
        match result {
            Err(shoal_fetch::FetchError::Sink(e)) if e == FULL => {
                info!(%folder, "stored folder reached its quota; fetching no more of it");
                break;
            }
            Err(shoal_fetch::FetchError::Sink(e)) => {
                warn!(%folder, error = %e, "storing a sealed chunk failed");
                break;
            }
            // What no peer has of this priority waits; the next priority's can still come.
            Err(e) => {
                debug!(%folder, priority = tier, error = %e, "fetching sealed chunks stopped")
            }
            Ok(_) => {}
        }
    }
    if fetched > 0 {
        debug!(%folder, chunks = fetched, "stored sealed chunks");
        inner.changed();
    }
    Ok(())
}

/// Removes the chunks no kept version lists any more, once their versions are dropped.
/// Returns how many it removed.
pub(crate) fn collect(state: &mut FolderState) -> usize {
    if state.sealed_store.is_none() {
        return 0;
    }
    // What it holds of an entry members keep from it stays: dropping is a user's to ask for.
    let keep: BTreeSet<ChunkId> = listed(state, false).into_iter().map(|(id, _)| id).collect();
    // And the list blobs no kept version names.
    let lists: BTreeSet<shoal_proto::ListId> = state
        .items
        .values()
        .filter_map(|item| match item {
            Item::Version(signed) => SealedVersion::decode(&signed.body).ok(),
            _ => None,
        })
        .flat_map(|sealed| match sealed.chunks {
            ChunkList::Lists(lists) => lists,
            ChunkList::Inline(_) => Vec::new(),
        })
        .collect();
    let gone: Vec<shoal_proto::ListId> = state
        .list_blobs
        .keys()
        .filter(|id| !lists.contains(id))
        .copied()
        .collect();
    for id in &gone {
        state.list_blobs.remove(id);
    }
    // And the priorities of entries it keeps no version of.
    let entries: BTreeSet<shoal_proto::EntryId> = state
        .items
        .values()
        .filter_map(|item| match item {
            Item::Version(signed) => SealedVersion::decode(&signed.body).ok(),
            _ => None,
        })
        .map(|sealed| sealed.entry)
        .collect();
    let before = state.storage_priorities.len();
    state
        .storage_priorities
        .retain(|entry, _| entries.contains(entry));
    if state.storage_priorities.len() != before {
        state.priorities_revision += 1;
    }
    state
        .sealed_store
        .as_mut()
        .map_or(0, |store| store.retain(&keep))
        + gone.len()
}
