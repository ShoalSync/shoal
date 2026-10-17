//! The engine's implementation of `net::Backend`.

use std::sync::Arc;

use tidefell_net::{Backend, IndexPage};
use tidefell_proto::identity::{DeviceCertificate, DeviceRevocation};
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

    fn now_ms(&self) -> i64 {
        self.0.now_ms()
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

    /// Only for a device whose user is a member of the folder: a certificate is served to those
    /// it vouches an author to (ADR 0024).
    fn certificate_of(
        &self,
        folder: &FolderId,
        device: DeviceId,
    ) -> Option<Signed<DeviceCertificate>> {
        let handle = self.0.folder(folder).ok()?;
        let certificate = if device == self.0.key.id() {
            self.0.certificate.clone()
        } else {
            self.0.vouching.lock().unwrap().certificate(device)?.clone()
        };
        let user = certificate.verify_certificate().ok()?.user;
        let member = handle.lock().members.contains(&user);
        member.then_some(certificate)
    }

    fn learn_certificate(&self, certificate: Signed<DeviceCertificate>) {
        self.0.learn_certificate(certificate);
    }

    fn revocations(&self) -> Vec<Signed<DeviceRevocation>> {
        self.0
            .vouching
            .lock()
            .unwrap()
            .revocations()
            .cloned()
            .collect()
    }

    fn learn_revocations(&self, revocations: Vec<Signed<DeviceRevocation>>) {
        // Taken over after the handshake that brought them, not during it (ADR 0040).
        if self.0.learn_revocations(revocations) {
            self.0.schedule_takeover(None);
        }
    }

    fn is_revoked(&self, certificate: &DeviceCertificate) -> bool {
        self.0.vouching.lock().unwrap().is_revoked(certificate)
    }

    fn stored_unknown_authors(&self, folder: &FolderId) -> Vec<DeviceId> {
        const AT_ONCE: usize = 64;
        let Ok(handle) = self.0.folder(folder) else {
            return Vec::new();
        };
        let me = self.0.key.id();
        // Authors are few: gathered first, then checked, so neither lock is held long with the
        // other.
        let authors: std::collections::BTreeSet<DeviceId> = {
            let state = handle.lock();
            state
                .meta
                .entries()
                .flat_map(|current| {
                    let conflicts: Vec<DeviceId> = state
                        .meta
                        .open_conflicts(&current.entry)
                        .map(|c| c.version.author)
                        .collect();
                    std::iter::once(current.author()).chain(conflicts)
                })
                .filter(|author| *author != me)
                .collect()
        };
        let vouching = self.0.vouching.lock().unwrap();
        authors
            .into_iter()
            .filter(|author| !vouching.knows(*author) && !vouching.unvouchable(*author))
            .take(AT_ONCE)
            .collect()
    }

    fn cannot_vouch(&self, folder: &FolderId, author: DeviceId) {
        if self.0.vouching.lock().unwrap().cannot_vouch(author) {
            self.0.schedule_takeover(Some(*folder));
        }
    }

    fn unknown_authors(&self, versions: &[Signed<EntryVersion>]) -> Vec<DeviceId> {
        let me = self.0.key.id();
        let vouching = self.0.vouching.lock().unwrap();
        let mut authors: Vec<DeviceId> = versions
            .iter()
            // A version's signer must be its author: a version where it is not is refused.
            .map(|v| DeviceId(*v.signer.as_bytes()))
            .filter(|author| *author != me && !vouching.knows(*author))
            .collect();
        authors.sort_unstable();
        authors.dedup();
        authors
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
        // Signed versions are read from the database, which has every committed one, rather
        // than kept in memory (ADR 0035).
        let sequences: Vec<u64> = versions
            .into_iter()
            .take_while(|s| s.sequence <= committed)
            .map(|s| s.sequence)
            .collect();
        let versions: Vec<Signed<EntryVersion>> = match state.manifests.reader() {
            Some(reader) => match reader.versions(&sequences) {
                Ok(versions) => versions,
                Err(e) => {
                    warn!(%folder, error = %e, "reading versions to send failed");
                    return None;
                }
            },
            None => Vec::new(),
        };
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
        if let Some(reason) = handle.lock().save_failed() {
            debug!(%folder, %peer, %reason, "not applying versions until saving works");
            return;
        }
        let mut changed = false;
        let mut conflicts = Vec::new();
        let mut touched = std::collections::BTreeSet::new();
        let takeover;
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
            let now = self.0.now_ms();
            // A folder being rebuilt receives its versions again, long after they first came:
            // their times then are not when they were received (ADR 0040).
            let rebuilding = state.rebuilding();
            state.meta.set_record_receipts(!rebuilding);
            for version in verified {
                let entry = Some(version.version().entry);
                let open = |state: &crate::state::FolderState| {
                    entry.map_or(0, |e| state.meta.open_conflicts(&e).count())
                };
                let before = open(&state);
                let author = version.version().author;
                // Only an author vouched for: this device, or one with a certificate from a
                // user who is a member now, not revoked (ADR 0024, ADR 0040).
                if author != self.0.key.id() {
                    let judged = self
                        .0
                        .vouching
                        .lock()
                        .unwrap()
                        .judge(*folder, author, |user| state.members.contains(user));
                    if let Err((why, first)) = judged {
                        if first {
                            tracing::info!(%folder, %author, ?why, "refused versions from an author not vouched for");
                        }
                        continue;
                    }
                }
                match state.meta.apply_verified(version, now) {
                    Ok(applied) if applied != tidefell_meta::Applied::Stale => {
                        changed = true;
                        touched.extend(entry);
                    }
                    Ok(_) => {}
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
            // A version that left one by an author no longer vouched for current or an open
            // conflict copy, as one concurrent with a put-back does: taken over (ADR 0040).
            let me = self.0.key.id();
            let mut vouching = self.0.vouching.lock().unwrap();
            takeover = touched.iter().any(|entry| {
                let current = state.meta.current(entry).map(|c| c.author());
                let conflicts = state.meta.open_conflicts(entry).map(|c| c.version.author);
                current.into_iter().chain(conflicts).any(|author| {
                    author != me
                        && vouching
                            .judge(*folder, author, |user| state.members.contains(user))
                            .is_err()
                })
            });
            drop(vouching);
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
        if takeover {
            self.0.schedule_takeover(Some(*folder));
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
                .insert(peer, self.0.now_ms());
            *handle.exchanged_at.lock().unwrap() = Some(std::time::Instant::now());
            // A takeover waiting for an exchange may go ahead (ADR 0040).
            if self.0.takeover_waiting.lock().unwrap().remove(folder) {
                self.0.schedule_takeover(Some(*folder));
            }
            let mut state = handle.lock();
            // Versions not applied for want of saving them are not acknowledged (ADR 0037).
            if state.save_failed().is_some() {
                return;
            }
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
        state.manifests.get(content).map(|m| (*m).clone())
    }

    fn chunk(&self, folder: &FolderId, hash: &ChunkHash) -> Option<Vec<u8>> {
        let handle = self.0.folder(folder).ok()?;
        if !handle.lock().manifests.is_referenced(hash) {
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
