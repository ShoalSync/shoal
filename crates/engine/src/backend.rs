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

    fn folders(&self) -> Vec<FolderId> {
        self.0.folder_ids()
    }

    fn is_member(&self, folder: &FolderId, peer: &DeviceCertificate) -> bool {
        self.0
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
        let (versions, up_to) = state.meta.index_since(epoch, since, limit);
        Some(IndexPage {
            epoch: state.meta.epoch(),
            up_to,
            versions: versions.into_iter().map(|s| s.signed.clone()).collect(),
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
        {
            let mut state = handle.lock();
            for version in versions {
                match state.meta.apply_remote(version) {
                    Ok(applied) => changed |= applied != tidefell_meta::Applied::Stale,
                    Err(e) => debug!(%peer, error = %e, "rejected a version"),
                }
            }
            if changed {
                if let Err(e) = state.save() {
                    warn!(folder = %folder, error = %e, "saving after remote changes failed");
                }
            }
        }
        if changed {
            self.0.emit(Event::FolderChanged(*folder));
            self.0.changed();
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
            let mut state = handle.lock();
            state.progress.insert(peer, (epoch, up_to));
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
        let referenced = self.0.folder(folder).ok()?.lock().index.is_referenced(hash);
        if !referenced {
            return None;
        }
        self.0.store.lock().unwrap().get(hash).ok().flatten()
    }

    fn availability(&self, folder: &FolderId, content: &ContentHash) -> Option<(u32, Vec<u8>)> {
        let manifest = self.manifest(folder, content)?;
        let availability = self.0.store.lock().unwrap().availability(&manifest);
        Some((availability.len() as u32, availability.as_bytes().to_vec()))
    }

    fn is_full_replica(&self, folder: &FolderId) -> Option<bool> {
        let handle = self.0.folder(folder).ok()?;
        // A replica that drops files or ignores other devices' changes does not hold
        // everything, so it never confirms copies as a full replica (ADR 0022, 0025).
        let full = handle.lock().config.holds_everything();
        Some(full)
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.0.changes.subscribe()
    }
}
