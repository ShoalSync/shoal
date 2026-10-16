//! Confirmed copies (ADR 0022): which full replicas hold the files this device made.

use std::collections::BTreeSet;
use std::sync::Arc;

use tidefell_meta::Current;
use tidefell_net::{EndpointId, Node};
use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ContentHash, DeviceId, FolderId, Manifest};
use tracing::{debug, info};

use crate::engine::{FolderHandle, Inner};
use crate::state::FolderState;
use crate::vouch::Vouching;
use crate::Result;

/// Most files checked with one peer per round, so a large backlog cannot monopolise a peer.
const MAX_CHECKS_PER_ROUND: usize = 256;

/// The devices that confirmed holding `content`, and when, that still count: those whose user,
/// by the certificate this device holds for them, is a member of the folder now. A removed
/// member's copies are not counted, nor those of a device whose certificate is not known until
/// it is met again (Phase 5c step 6).
pub(crate) fn confirmed(
    state: &FolderState,
    vouching: &Vouching,
    content: &ContentHash,
) -> Vec<(DeviceId, i64)> {
    state
        .copies
        .get(content)
        .map(|devices| {
            devices
                .iter()
                .filter(|(device, _)| {
                    vouching
                        .user_of(**device)
                        .is_some_and(|user| state.members.contains(&user))
                })
                .map(|(device, at)| (*device, *at))
                .collect()
        })
        .unwrap_or_default()
}

/// Content of the current versions and conflict copies of `state` that this device made and
/// that no full replica that counts has confirmed yet, with their manifests: at most `limit`,
/// the lowest first. Only those manifests are read: a large folder's would not fit in memory
/// (ADR 0035).
fn unconfirmed(
    state: &FolderState,
    vouching: &Vouching,
    me: DeviceId,
    limit: usize,
) -> Vec<Manifest> {
    let mut lowest = BTreeSet::new();
    let mut consider = |author: DeviceId, kind: &EntryKind| {
        if author != me {
            return;
        }
        let EntryKind::File { content, .. } = *kind else {
            return;
        };
        if !confirmed(state, vouching, &content).is_empty() {
            return;
        }
        if lowest.len() == limit && lowest.last().is_some_and(|last| content > *last) {
            return;
        }
        lowest.insert(content);
        if lowest.len() > limit {
            lowest.pop_last();
        }
    };
    for version in state.meta.entries() {
        consider(version.author(), version.kind);
        for conflict in state.meta.conflicts(&version.entry) {
            consider(conflict.version.author, &conflict.version.kind);
        }
    }
    lowest
        .into_iter()
        .filter_map(|content| state.manifests.get(&content))
        .map(|manifest| (*manifest).clone())
        .collect()
}

/// True if this device made `version` and no full replica that counts has confirmed its
/// content.
pub(crate) fn only_here(
    state: &FolderState,
    vouching: &Vouching,
    me: DeviceId,
    version: Current<'_>,
) -> bool {
    match *version.kind {
        EntryKind::File { content, .. } => {
            version.author() == me && confirmed(state, vouching, &content).is_empty()
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
    let (pending, members) = {
        let state = handle.lock();
        let vouching = inner.vouching.lock().unwrap();
        (
            unconfirmed(&state, &vouching, inner.key.id(), MAX_CHECKS_PER_ROUND),
            state.members.clone(),
        )
    };
    if pending.is_empty() {
        return Ok(());
    }
    let mut confirmed: Vec<(ContentHash, DeviceId)> = Vec::new();
    let mut names: Vec<(DeviceId, String)> = Vec::new();
    for peer in node.peers() {
        if !peer.shared_folders().contains(&id) {
            continue;
        }
        let device = peer.device();
        // Only a current member's copy counts.
        if !members.contains(&peer.certificate().user) {
            continue;
        }
        let Ok(endpoint) = EndpointId::from_bytes(device.as_bytes()) else {
            continue;
        };
        let full = node.is_full_replica(endpoint, id).await;
        inner.remember_replica(device, id, matches!(full, Ok(true)));
        match full {
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
    let now = inner.now_ms();
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
