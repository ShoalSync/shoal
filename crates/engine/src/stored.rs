//! What a storage peer keeps of a stored folder's content (ADR 0023, "On storage peers"):
//! sealed chunks by chunk ID, as fetched, which it can neither open nor check. Members check
//! every chunk they fetch from it by its AEAD tag.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tidefell_proto::sync2::{ChunkList, Item, SealedVersion};
use tidefell_proto::{ChunkId, DeviceId, SignedKind as _};
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

/// The chunk IDs the kept sealed versions list, the current ones first: what the folder's
/// store must hold.
fn listed(state: &FolderState, current_only: bool) -> Vec<ChunkId> {
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    for pass in [false, true] {
        if pass && current_only {
            break;
        }
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
            // List blobs are not kept yet; files of more than 4096 chunks are not stored whole.
            let ChunkList::Inline(ids) = sealed.chunks else {
                continue;
            };
            for id in ids {
                if seen.insert(id) {
                    order.push(id);
                }
            }
        }
    }
    order
}

/// How many chunks of the folder's current versions it lacks.
pub(crate) fn missing(state: &FolderState) -> usize {
    let Some(store) = state.sealed_store.as_ref() else {
        return 0;
    };
    listed(state, true)
        .iter()
        .filter(|id| !store.has(id))
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

/// Fetches, from the peers sharing the folder, the chunks its kept versions list that it lacks,
/// the current versions' first, until it has them or its quota is reached.
pub(crate) async fn fetch(
    inner: &std::sync::Arc<Inner>,
    node: &std::sync::Arc<tidefell_net::Node>,
    handle: &FolderHandle,
) -> Result<()> {
    use tidefell_net::{BlobKind, BlobResult};
    use tidefell_proto::blobs2::BlobId;

    let (folder, mut wanted) = {
        let state = handle.lock();
        let Some(store) = state.sealed_store.as_ref() else {
            return Ok(());
        };
        if over_quota(&state) {
            return Ok(());
        }
        let wanted: Vec<ChunkId> = listed(&state, false)
            .into_iter()
            .filter(|id| !store.has(id))
            .collect();
        (state.config.id, wanted)
    };
    if wanted.is_empty() {
        return Ok(());
    }
    let peers: Vec<DeviceId> = node
        .encrypted_peers()
        .into_iter()
        .filter(|p| p.shared_folders().contains(&folder))
        .map(|p| p.device())
        .collect();
    let mut fetched = 0usize;
    for peer in peers {
        if wanted.is_empty() {
            break;
        }
        let Ok(endpoint) = tidefell_net::EndpointId::from_bytes(peer.as_bytes()) else {
            continue;
        };
        let mut got = BTreeSet::new();
        for batch in wanted.chunks(tidefell_proto::blobs2::MAX_BLOBS_PER_REQUEST) {
            let request: Vec<(BlobId, BlobKind)> = batch
                .iter()
                .map(|id| (BlobId::from(*id), BlobKind::SealedChunk))
                .collect();
            let results = match node.fetch_blobs(endpoint, folder, &request).await {
                Ok(results) => results,
                Err(e) => {
                    debug!(%folder, peer = %peer, error = %e, "fetching sealed chunks failed");
                    break;
                }
            };
            let mut state = handle.lock();
            for result in results {
                let BlobResult::Found(id, sealed) = result else {
                    continue;
                };
                let id = ChunkId(id.0);
                if !batch.contains(&id) || over_quota(&state) {
                    continue;
                }
                let Some(store) = state.sealed_store.as_mut() else {
                    continue;
                };
                match store.put(id, &sealed) {
                    Ok(()) => {
                        got.insert(id);
                        fetched += 1;
                    }
                    Err(e) => warn!(%folder, error = %e, "storing a sealed chunk failed"),
                }
            }
            if over_quota(&state) {
                info!(%folder, "stored folder reached its quota; fetching no more of it");
                wanted.clear();
                break;
            }
        }
        wanted.retain(|id| !got.contains(id));
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
    let keep: BTreeSet<ChunkId> = listed(state, false).into_iter().collect();
    state
        .sealed_store
        .as_mut()
        .map_or(0, |store| store.retain(&keep))
}
