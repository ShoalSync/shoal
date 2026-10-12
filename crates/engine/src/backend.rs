//! The engine's implementation of `net::Backend`.

use std::sync::Arc;

use tidefell_net::{Backend, IndexPage};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::sync::EntryVersion;
use tidefell_proto::{ChunkHash, ContentHash, DeviceId, FolderId, Manifest, Signed};
use tokio::sync::watch;
use tracing::{debug, warn};

use crate::engine::Inner;
use crate::views::Event;

pub(crate) struct EngineBackend(pub(crate) Arc<Inner>);

impl Backend for EngineBackend {
    fn certificate(&self) -> Signed<DeviceCertificate> {
        self.0.certificate.clone()
    }

    /// Paused folders are not offered, so peers exchange nothing for them (ADR 0029).
    fn folders(&self) -> Vec<FolderId> {
        self.0
            .folder_ids()
            .into_iter()
            .filter(|id| !self.0.is_paused(id))
            .collect()
    }

    /// Nothing of a paused folder is served, content included.
    fn is_member(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool {
        !self.0.is_paused(folder)
            && self
                .0
                .folder(folder)
                .is_ok_and(|h| h.lock().members.contains(&peer.user))
    }

    fn index_since(
        &self,
        folder: &FolderId,
        epoch: u64,
        since: u64,
        limit: usize,
    ) -> Option<IndexPage> {
        let handle = self.0.folder(folder).ok()?;
        let state = handle.lock();
        // Only versions on disk, so a peer is never told of one a crash here could lose
        // (ADR 0034); the rest follow once they are committed.
        let committed = state.committed();
        let (versions, up_to) = state.meta.index_since(epoch, since, limit);
        let versions: Vec<Signed<EntryVersion>> = versions
            .into_iter()
            .take_while(|s| s.sequence <= committed)
            .map(|s| s.signed.clone())
            .collect();
        Some(IndexPage {
            epoch: state.meta.epoch(),
            up_to: up_to.min(committed),
            versions,
        })
    }

    fn apply_versions(
        &self,
        folder: &FolderId,
        peer: DeviceId,
        versions: Vec<Signed<EntryVersion>>,
    ) {
        let Ok(handle) = self.0.folder(folder) else {
            return;
        };
        let mut changed = false;
        let mut conflicts = Vec::new();
        // Checking signatures is most of the work, and needs no lock (ADR 0034).
        let verified: Vec<tidefell_meta::Verified> = versions
            .into_iter()
            .filter_map(
                |version| match tidefell_meta::verify_remote(*folder, version) {
                    Ok(verified) => Some(verified),
                    Err(e) => {
                        debug!(%peer, error = %e, "rejected a version");
                        None
                    }
                },
            )
            .collect();
        {
            let mut state = handle.lock();
            let now = crate::now_ms();
            for version in verified {
                let entry = Some(version.version().entry);
                let open = |state: &crate::state::FolderState| {
                    entry.map_or(0, |e| state.meta.open_conflicts(&e).count())
                };
                let before = open(&state);
                match state.meta.apply_verified(version, now) {
                    Ok(applied) => changed |= applied != tidefell_meta::Applied::Stale,
                    Err(e) => debug!(%peer, error = %e, "rejected a version"),
                }
                if open(&state) > before {
                    conflicts.extend(
                        entry
                            .and_then(|e| state.meta.disk_path(&e))
                            .map(String::from),
                    );
                }
            }
            if changed {
                if let Err(e) = state.save() {
                    warn!(folder = %folder, error = %e, "saving after remote changes failed");
                }
                // Files already on disk are matched to the new entries (ADR 0034).
                if state.rebuilding() {
                    handle.rescan.notify_one();
                }
            }
        }
        if changed {
            self.0.emit(Event::FolderChanged(*folder));
            self.0.changed();
        }
        for path in conflicts {
            debug!(folder = %folder, %path, "conflict");
            self.0.emit(Event::ConflictDetected {
                folder: *folder,
                path,
            });
        }
    }

    fn peer_progress(&self, folder: &FolderId, peer: DeviceId) -> (u64, u64) {
        self.0
            .folder(folder)
            .ok()
            .and_then(|h| h.lock().progress.get(&peer).copied())
            .unwrap_or((0, 0))
    }

    fn set_peer_progress(&self, folder: &FolderId, peer: DeviceId, epoch: u64, up_to: u64) {
        if let Ok(handle) = self.0.folder(folder) {
            // The peer's index is all here: the exchange finished (ADR 0030).
            handle
                .activity
                .lock()
                .unwrap()
                .exchanged
                .insert(peer, crate::now_ms());
            let mut state = handle.lock();
            state.progress.insert(peer, (epoch, up_to));
            // A folder being rebuilt has everything this peer knows: the next scan matches
            // the last of it and ends the rebuild (ADR 0034).
            if state.rebuilding() {
                handle.rescan.notify_one();
            }
            if let Err(e) = state.save() {
                warn!(folder = %folder, error = %e, "saving peer progress failed");
            }
        }
    }

    fn manifest(&self, folder: &FolderId, content: &ContentHash) -> Option<Manifest> {
        let handle = self.0.folder(folder).ok()?;
        let state = handle.lock();
        state.manifests.get(content).cloned()
    }

    fn chunk(&self, folder: &FolderId, hash: &ChunkHash) -> Option<Vec<u8>> {
        let handle = self.0.folder(folder).ok()?;
        if !handle.lock().index.is_referenced(hash) {
            return None;
        }
        // From the block store, or from a file on disk that holds it (ADR 0027).
        crate::content::read_chunk(&self.0, &handle, hash)
    }

    fn availability(&self, folder: &FolderId, content: &ContentHash) -> Option<(u32, Vec<u8>)> {
        let handle = self.0.folder(folder).ok()?;
        let state = handle.lock();
        let manifest = state.manifests.get(content)?;
        let availability = tidefell_store::Availability::from_fn(manifest.chunks.len(), |i| {
            crate::content::has_chunk(&self.0, &state, &manifest.chunks[i].hash)
        });
        Some((availability.len() as u32, availability.as_bytes().to_vec()))
    }

    fn is_full_replica(&self, folder: &FolderId) -> Option<bool> {
        let handle = self.0.folder(folder).ok()?;
        // A replica that drops files or ignores other devices' changes does not hold
        // everything, so it never confirms copies as a full replica (ADR 0022, 0025).
        // Nor does one being rebuilt (ADR 0034).
        let state = handle.lock();
        let full = state.config.holds_everything() && !state.rebuilding();
        Some(full)
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.0.changes.subscribe()
    }
}
