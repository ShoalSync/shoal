//! Taking over the versions of authors no longer vouched for (ADR 0040, ADR 0024): a revoked
//! device, a device of a user who is no longer a member, or one a peer answered it cannot vouch
//! for. Their versions are refused everywhere; so that the files stay, each replica makes a
//! change of its own with the same path, content and modification time for every current
//! version or open conflict copy of theirs whose content it holds, building on that version.
//! Replicas that take over the same entry make concurrent versions with the same content, which
//! need no decision.
//!
//! A version whose content this replica does not hold it leaves alone, with one exception: a
//! revoked device's, once this replica has had a whole index from a peer since it learnt of the
//! revocation. Revocations reach every replica before anything else is exchanged, so by then any
//! peer holding the content has taken it over, and its live version wins over the delete this
//! replica makes. A member removed on one replica only, or an author whose certificate is merely
//! missing here, never leads to a delete: member lists are each replica's own.
//!
//! It is idempotent: once done, nothing it would act on is left, so running it again, as after
//! a restart part way, finishes it. Nothing is taken over while a folder is rebuilt, when what is
//! on disk is not matched to its entries yet (ADR 0034).

use std::collections::BTreeMap;

use tidefell_proto::sync::EntryKind;
use tidefell_proto::{DeviceId, EntryId, VersionVector};
use tracing::{info, warn};

use crate::engine::{FolderHandle, Inner};
use crate::vouch::Unvouched;
use crate::Result;

/// A version to take over or withdraw: its entry, the vector to build on, and what to keep.
struct Target {
    entry: EntryId,
    base: VersionVector,
    path: String,
    kind: EntryKind,
    modified_ms: i64,
    author: DeviceId,
    /// Its author is revoked: withdrawn once an exchange allows, if not held.
    revoked: bool,
}

/// What to do with an author's versions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Keep,
    /// Take over what is held; leave the rest.
    TakeOver,
    /// Take over what is held; withdraw the rest once an exchange allows.
    Revoked,
}

/// Takes over, in the folder of `handle`, the current versions and open conflict copies of
/// authors no longer vouched for (see the module's documentation). Returns how many versions
/// it made.
pub(crate) fn take_over(inner: &Inner, handle: &FolderHandle) -> Result<usize> {
    let me = inner.key.id();
    let exchanged: Vec<i64> = handle
        .activity
        .lock()
        .unwrap()
        .exchanged
        .values()
        .copied()
        .collect();
    let made = {
        let mut state = handle.lock();
        if state.rebuilding() {
            return Ok(0);
        }
        // Each author judged once; the vouching lock taken after the folder's, as everywhere.
        let mut verdicts: BTreeMap<DeviceId, Verdict> = BTreeMap::new();
        let mut verdict = |author: DeviceId, state: &crate::state::FolderState| {
            if author == me {
                return Verdict::Keep;
            }
            *verdicts.entry(author).or_insert_with(|| {
                let mut vouching = inner.vouching.lock().unwrap();
                let judged =
                    vouching.judge(state.config.id, author, |user| state.members.contains(user));
                match judged {
                    Ok(()) => Verdict::Keep,
                    Err((Unvouched::Revoked, _)) => Verdict::Revoked,
                    Err((Unvouched::NotAMember, _)) => Verdict::TakeOver,
                    // Missing here is not enough: a peer must have said it cannot vouch.
                    Err((Unvouched::NoCertificate, _)) if vouching.unvouchable(author) => {
                        Verdict::TakeOver
                    }
                    Err((Unvouched::NoCertificate, _)) => Verdict::Keep,
                }
            })
        };
        let mut targets = Vec::new();
        for current in state.meta.entries() {
            let judged = verdict(current.author(), &state);
            if judged != Verdict::Keep {
                targets.push(Target {
                    entry: current.entry,
                    base: state.meta.current_vector(&current.entry),
                    path: current.path.to_owned(),
                    kind: current.kind.clone(),
                    modified_ms: current.modified_ms,
                    author: current.author(),
                    revoked: judged == Verdict::Revoked,
                });
            }
            for conflict in state.meta.open_conflicts(&current.entry) {
                let version = &conflict.version;
                let judged = verdict(version.author, &state);
                if judged != Verdict::Keep {
                    targets.push(Target {
                        entry: version.entry,
                        base: version.version.clone(),
                        path: version.path.clone(),
                        kind: version.kind.clone(),
                        modified_ms: version.modified_ms,
                        author: version.author,
                        revoked: judged == Verdict::Revoked,
                    });
                }
            }
        }
        let mut made = 0;
        let mut waiting = false;
        for target in targets {
            // Only content this replica holds, every chunk of it, is taken over.
            let held = match &target.kind {
                EntryKind::File { content, .. } => state.manifests.get(content).is_some_and(|m| {
                    m.chunks
                        .iter()
                        .all(|c| crate::content::has_chunk(inner, &state, &c.hash))
                }),
                _ => true,
            };
            let (kind, modified_ms) = if held {
                (target.kind, target.modified_ms)
            } else if target.revoked {
                // Withdrawn only once a peer's whole index has come since the revocation was
                // learnt: a peer holding the content has taken it over by then.
                let learnt = inner.vouching.lock().unwrap().revoked_since(target.author);
                let after = learnt.is_some_and(|learnt| exchanged.iter().any(|t| *t >= learnt));
                if !after {
                    waiting = true;
                    continue;
                }
                (EntryKind::Tombstone, inner.now_ms())
            } else {
                continue;
            };
            match state.meta.local_change_from(
                inner.key.signing_key(),
                target.entry,
                &target.base,
                &target.path,
                kind,
                modified_ms,
            ) {
                Ok(_) => made += 1,
                Err(e) => {
                    warn!(folder = %state.config.id, path = %target.path, error = %e, "taking over a version failed")
                }
            }
        }
        if waiting {
            inner
                .takeover_waiting
                .lock()
                .unwrap()
                .insert(state.config.id);
        }
        if made > 0 {
            // Files on disk as they were now correspond to this device's versions.
            state.catch_up_bases();
            state.save()?;
            info!(folder = %state.config.id, versions = made, "took over versions of authors no longer vouched for");
        }
        made
    };
    if made > 0 {
        inner.changes.send_modify(|n| *n += 1);
    }
    Ok(made)
}
