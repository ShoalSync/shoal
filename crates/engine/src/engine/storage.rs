//! What a folder's storage peers keep, as members see it, and dropping from one when a user asks
//! (ADR 0023's amendment of 2026-10-22).

use std::collections::{BTreeMap, HashSet};

use tidefell_proto::blobs2::{BlobId, MAX_BLOBS_PER_AVAILABILITY};
use tidefell_proto::storage::DEFAULT_PRIORITY;
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ChunkId, ContentHash, DeviceId, FolderId};

use super::Engine;
use crate::priorities::{Rules, PRIORITIES_FILE};
use crate::views::{StorageDrop, StorageLevel, StoragePeerReport, StorageUsage};
use crate::{copies, Error, Result};

/// A current file of an encrypted folder: its path, priority, content, and chunks with their
/// lengths.
struct Ranked {
    path: String,
    priority: u8,
    content: ContentHash,
    chunks: Vec<(ChunkId, u32)>,
}

impl Engine {
    /// The folder's rules: from disk, or fetched into memory where this device does not have
    /// them on disk, as a partial replica may not; none if the folder has no such file.
    async fn rules(&self, folder: FolderId) -> Result<Option<Rules>> {
        let (root, listed) = {
            let handle = self.inner.folder(&folder)?;
            let state = handle.lock();
            let listed = state.meta.find_path(PRIORITIES_FILE).is_some();
            (state.config.path.clone(), listed)
        };
        if let Ok(text) = std::fs::read_to_string(root.join(PRIORITIES_FILE)) {
            return Ok(Some(Rules::parse(&root, &text)));
        }
        if !listed {
            return Ok(None);
        }
        let mut text = Vec::new();
        self.fetch(folder, PRIORITIES_FILE, None, &mut |_, data| {
            text.extend_from_slice(data);
            Ok(())
        })
        .await?;
        Ok(Some(Rules::parse(&root, &String::from_utf8_lossy(&text))))
    }

    /// The folder's current files with their priorities, and their chunks.
    async fn ranked(&self, folder: FolderId) -> Result<(Vec<Ranked>, Vec<DeviceId>)> {
        let rules = self.rules(folder).await?;
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let Some(encryption) = state.config.encryption.as_ref() else {
            return Err(Error::Invalid("the folder is not encrypted".into()));
        };
        if state.config.is_stored() {
            return Err(Error::Invalid(
                "this device stores the folder; its members rank it".into(),
            ));
        }
        let files = state
            .meta
            .entries()
            .filter_map(|v| {
                let EntryKind::File { content, .. } = v.kind else {
                    return None;
                };
                let chunks = state
                    .sealed_chunks
                    .iter()
                    .find(|((c, _), _)| c == content)
                    .map(|(_, list)| list.clone())
                    .unwrap_or_default();
                Some(Ranked {
                    path: v.path.to_owned(),
                    priority: rules
                        .as_ref()
                        .map_or(DEFAULT_PRIORITY, |r| r.priority(v.path)),
                    content: *content,
                    chunks,
                })
            })
            .collect();
        Ok((files, encryption.storage_peers.clone()))
    }

    /// What each of an encrypted folder's storage peers keeps of it: its quota and room, and
    /// for each priority, the files and their bytes against what it holds of them. Asks each
    /// peer connected; one that is not, or that does not say, is reported as such.
    pub async fn storage_report(&self, folder: FolderId) -> Result<Vec<StoragePeerReport>> {
        let (files, peers) = self.ranked(folder).await?;
        let ids: Vec<ChunkId> = {
            let mut seen = HashSet::new();
            files
                .iter()
                .flat_map(|f| f.chunks.iter().map(|(id, _)| *id))
                .filter(|id| seen.insert(*id))
                .collect()
        };
        let mut reports = Vec::new();
        for peer in peers {
            let mut report = StoragePeerReport {
                device: peer,
                connected: self.node.encrypted_peer(&peer).is_some(),
                usage: None,
                levels: Vec::new(),
                error: None,
            };
            if report.connected {
                match self.ask_usage(peer, folder, &ids).await {
                    Ok((usage, held)) => {
                        report.usage = Some(usage);
                        report.levels = levels(&files, &held);
                    }
                    Err(e) => report.error = Some(e),
                }
            }
            reports.push(report);
        }
        Ok(reports)
    }

    /// A storage peer's usage, and which of `ids` it holds.
    async fn ask_usage(
        &self,
        peer: DeviceId,
        folder: FolderId,
        ids: &[ChunkId],
    ) -> std::result::Result<(StorageUsage, HashSet<ChunkId>), String> {
        let endpoint =
            tidefell_net::EndpointId::from_bytes(peer.as_bytes()).map_err(|e| e.to_string())?;
        let (quota_bytes, held_bytes, room_bytes) = self
            .node
            .storage_usage(endpoint, folder)
            .await
            .map_err(|e| e.to_string())?;
        let mut held = HashSet::new();
        for part in ids.chunks(MAX_BLOBS_PER_AVAILABILITY) {
            let blobs: Vec<BlobId> = part.iter().map(|id| BlobId::from(*id)).collect();
            let holds = self
                .node
                .blob_availability(endpoint, folder, &blobs)
                .await
                .map_err(|e| e.to_string())?;
            held.extend(
                part.iter()
                    .zip(holds)
                    .filter(|(_, h)| *h)
                    .map(|(id, _)| *id),
            );
        }
        Ok((
            StorageUsage {
                quota_bytes,
                held_bytes,
                room_bytes,
            },
            held,
        ))
    }

    /// Asks storage peer `peer` to forget what it holds of the folder's files ranked below
    /// `below`, as a user asked: never done otherwise. Unless `force`, it first checks that
    /// each of those files has a copy other than on that storage peer, on this device or one
    /// confirmed (ADR 0022), and drops nothing if one has none, listing them.
    pub async fn storage_drop(
        &self,
        folder: FolderId,
        peer: DeviceId,
        below: u8,
        force: bool,
    ) -> Result<StorageDrop> {
        let (files, peers) = self.ranked(folder).await?;
        if !peers.contains(&peer) {
            return Err(Error::Invalid(format!(
                "{peer} is not a storage peer of the folder"
            )));
        }
        let at_risk: Vec<String> = {
            let handle = self.inner.folder(&folder)?;
            let state = handle.lock();
            let vouching = self.inner.vouching.lock().unwrap();
            files
                .iter()
                .filter(|f| f.priority < below)
                .filter(|f| {
                    let here = state.local.iter().any(|(_, l)| l.content == f.content);
                    let elsewhere = copies::confirmed(&state, &vouching, &f.content)
                        .iter()
                        .any(|(device, _)| *device != peer);
                    !here && !elsewhere
                })
                .map(|f| f.path.clone())
                .collect()
        };
        if !at_risk.is_empty() && !force {
            return Ok(StorageDrop {
                dropped_chunks: 0,
                dropped_bytes: 0,
                at_risk,
                done: false,
            });
        }
        let endpoint = tidefell_net::EndpointId::from_bytes(peer.as_bytes())
            .map_err(|e| Error::Invalid(e.to_string()))?;
        let (dropped_chunks, dropped_bytes) = self
            .node
            .storage_drop(endpoint, folder, below)
            .await
            .map_err(|e| Error::Unavailable(format!("the storage peer: {e}")))?;
        Ok(StorageDrop {
            dropped_chunks,
            dropped_bytes,
            at_risk,
            done: true,
        })
    }
}

/// For each priority, highest first, the files, their bytes and what of them `held` covers.
fn levels(files: &[Ranked], held: &HashSet<ChunkId>) -> Vec<StorageLevel> {
    let mut by: BTreeMap<u8, StorageLevel> = BTreeMap::new();
    for file in files {
        let level = by.entry(file.priority).or_insert(StorageLevel {
            priority: file.priority,
            files: 0,
            bytes: 0,
            held_bytes: 0,
        });
        level.files += 1;
        for (id, len) in &file.chunks {
            level.bytes += u64::from(*len);
            if held.contains(id) {
                level.held_bytes += u64::from(*len);
            }
        }
    }
    by.into_values().rev().collect()
}
