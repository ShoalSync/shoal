//! Confirmed copies (ADR 0022): which full replicas hold the files this device made.

use std::collections::BTreeMap;
use std::sync::Arc;

use tidefell_net::{EndpointId, Node};
use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::{ContentHash, DeviceId, FolderId, Manifest};
use tracing::{debug, info};

use crate::engine::{FolderHandle, Inner};
use crate::state::FolderState;
use crate::{now_ms, Result};

/// Most files checked with one peer per round, so a large backlog cannot monopolise a peer.
const MAX_CHECKS_PER_ROUND: usize = 256;

/// Content of the current versions and conflict copies of `state` that this device made and
/// that no full replica has confirmed yet, with their manifests.
fn unconfirmed(state: &FolderState, me: DeviceId) -> Vec<Manifest> {
    let mut out = BTreeMap::new();
    let mut consider = |version: &EntryVersion| {
        if version.author != me {
            return;
        }
        let EntryKind::File { content, .. } = version.kind else {
            return;
        };
        if state.copies.contains_key(&content) {
            return;
        }
        if let Some(manifest) = state.manifests.get(&content) {
            out.insert(content, manifest.clone());
        }
    };
    for version in state.meta.entries() {
        consider(version);
        for conflict in state.meta.conflicts(&version.entry) {
            consider(&conflict.version);
        }
    }
    out.into_values().collect()
}

/// True if this device made `version` and no full replica has confirmed its content.
pub(crate) fn only_here(state: &FolderState, me: DeviceId, version: &EntryVersion) -> bool {
    match version.kind {
        EntryKind::File { content, .. } => {
            version.author == me && state.copies.get(&content).is_none_or(|d| d.is_empty())
        }
        _ => false,
    }
}

/// True if a peer's availability answer covers every chunk of `manifest`.
fn holds_all(manifest: &Manifest, count: u32, bitmap: &[u8]) -> bool {
    count as usize == manifest.chunks.len()
        && (0..manifest.chunks.len())
            .all(|i| bitmap.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0))
}

/// Asks connected full replicas of folder `id` whether they hold the files this device made
/// that have no confirmed copy yet, and records those that do.
pub(crate) async fn confirm(
    inner: &Arc<Inner>,
    node: &Arc<Node>,
    id: FolderId,
    handle: &FolderHandle,
) -> Result<()> {
    let mut pending = unconfirmed(&handle.lock(), inner.key.id());
    if pending.is_empty() {
        return Ok(());
    }
    pending.truncate(MAX_CHECKS_PER_ROUND);
    let mut confirmed: Vec<(ContentHash, DeviceId)> = Vec::new();
    let mut names: Vec<(DeviceId, String)> = Vec::new();
    for peer in node.peers() {
        if !peer.shared_folders().contains(&id) {
            continue;
        }
        let device = peer.device();
        let Ok(endpoint) = EndpointId::from_bytes(device.as_bytes()) else {
            continue;
        };
        match node.is_full_replica(endpoint, id).await {
            Ok(true) => names.push((device, peer.certificate().name.clone())),
            Ok(false) => continue,
            Err(e) => {
                debug!(folder = %id, peer = %device, error = %e, "not counted as a full replica");
                continue;
            }
        }
        for manifest in &pending {
            if confirmed.iter().any(|(c, _)| *c == manifest.content) {
                continue;
            }
            if let Ok((count, bitmap)) = node.availability(endpoint, id, manifest.content).await {
                if holds_all(manifest, count, &bitmap) {
                    confirmed.push((manifest.content, device));
                }
            }
        }
    }
    if confirmed.is_empty() {
        return Ok(());
    }
    let now = now_ms();
    let mut state = handle.lock();
    for (content, device) in &confirmed {
        state
            .copies
            .entry(*content)
            .or_default()
            .insert(*device, now);
    }
    for (device, name) in names {
        if confirmed.iter().any(|(_, d)| *d == device) {
            state.copy_names.insert(device, name);
        }
    }
    state.save()?;
    info!(folder = %id, files = confirmed.len(), "confirmed copies on full replicas");
    Ok(())
}

#[cfg(test)]
mod tests {
    use tidefell_proto::{ChunkHash, ChunkRef};

    use super::*;

    fn manifest(chunks: usize) -> Manifest {
        Manifest {
            content: ContentHash([1; 32]),
            chunks: (0..chunks)
                .map(|i| ChunkRef {
                    hash: ChunkHash([i as u8; 32]),
                    len: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn holds_all_needs_every_bit_and_the_right_count() {
        let m = manifest(10);
        assert!(holds_all(&m, 10, &[0xff, 0x03]));
        assert!(!holds_all(&m, 10, &[0xff, 0x01]));
        assert!(!holds_all(&m, 9, &[0xff, 0x03]));
        assert!(!holds_all(&m, 10, &[0xff]));
        assert!(holds_all(&manifest(0), 0, &[]));
    }
}
