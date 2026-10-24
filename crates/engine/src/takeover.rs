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
//! With a time of loss, a revoked device's versions this replica received at or after it are
//! withdrawn: what each changed is put back from history where it was kept here, or the file is
//! deleted by the same rule as above. Versions it cannot place before or after the time it
//! leaves until it has had a peer's whole index since the revocation, which would have
//! superseded them with another replica's withdrawal if one could judge; then it takes them over
//! if it holds them, never deletes them, so replicas that cannot judge still agree (see
//! [`crate::loss`]). A folder's withdrawals that would remove as many files as a scan's held
//! deletes (ADR 0037) wait for the user, who withdraws them or keeps them.
//!
//! Content is taken over only after it is read back whole: the records of which files hold it
//! can be behind the disk, and a version no device can supply would never settle.
//!
//! It is idempotent: once done, nothing it would act on is left, so running it again, as after
//! a restart part way, finishes it. Nothing is taken over while a folder is rebuilt, when what is
//! on disk is not matched to its entries yet (ADR 0034).

use std::collections::{BTreeMap, BTreeSet};

use tidefell_proto::sync::EntryKind;
use tidefell_proto::{ContentHash, DeviceId, EntryId, FolderId, VersionVector};
use tracing::{info, warn};

use crate::engine::{FolderHandle, Inner};
use crate::history::Previous;
use crate::loss::Received;
use crate::state::FolderState;
use crate::vouch::Unvouched;
use crate::Result;

/// A version to take over or withdraw: its entry, the vector to build on, and what to keep.
#[derive(Clone)]
struct Target {
    entry: EntryId,
    base: VersionVector,
    path: String,
    kind: EntryKind,
    modified_ms: i64,
    author: DeviceId,
    sequence: u64,
    verdict: Verdict,
    /// An open conflict copy rather than the current version.
    conflict: bool,
}

/// What to do with an author's versions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Keep,
    /// Take over what is held; leave the rest.
    TakeOver,
    /// Take over what is held; withdraw the rest once an exchange allows. With a time of loss,
    /// what was received after it is withdrawn too. `learnt_ms` is when this replica learnt of
    /// the revocation, by its own clock.
    Revoked {
        lost_ms: Option<i64>,
        learnt_ms: Option<i64>,
        learnt_at: Option<std::time::Instant>,
    },
}

/// What happens to one version.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    TakeOver,
    /// Deleted: no earlier content to put back here.
    Withdraw,
    /// The content its first change after the loss replaced is put back, from history.
    PutBack,
}

/// A folder's withdrawals waiting for the user (ADR 0040): too many to make unasked.
#[derive(Debug, Clone, Default)]
pub(crate) struct Held {
    /// The live files that withdrawing would remove.
    pub(crate) paths: Vec<String>,
    /// The user's decision, once made: true to withdraw them, false to keep them (take them
    /// over where held).
    pub(crate) withdraw: Option<bool>,
}

/// Held withdrawals, by folder and revoked device. In memory: a decision acted on changes the
/// folder, so after a restart there is nothing left to decide, or the question comes again.
pub(crate) type HeldWithdrawals = BTreeMap<(FolderId, DeviceId), Held>;

/// A put-back to make once the folder's lock is let go: reading the kept file takes time.
struct PutBack {
    entry: EntryId,
    base: VersionVector,
    path: String,
    previous: Previous,
}

/// What [`decide`] settled: the versions to act on, and what to say about the rest.
#[derive(Default)]
struct Plan {
    /// Each target with its action, and for a put-back what to put back.
    acts: Vec<(Target, Action, Option<Previous>)>,
    /// Something waits for an exchange, or for content to read.
    waiting: bool,
}

/// The content to put back for a version of `author` received after `lost_ms`: what this
/// device kept in history when the first of that author's changes received since replaced it.
pub(crate) fn put_back_from(
    state: &crate::state::FolderState,
    entry: &EntryId,
    author: DeviceId,
    lost_ms: i64,
) -> Option<Previous> {
    state
        .history
        .get(entry)?
        .iter()
        .filter_map(|previous| {
            let (by, at, sequence) = state.replaced.get(&previous.file)?;
            // A time not known when it was kept is in the replacing version's row by now.
            let at = at.or_else(|| {
                state
                    .manifests
                    .reader()
                    .and_then(|r| r.received(&[*sequence]).ok())
                    .and_then(|times| times.first().copied().flatten())
            })?;
            (*by == author && at >= lost_ms).then_some((at, previous))
        })
        .min_by_key(|(at, _)| *at)
        .map(|(_, previous)| previous.clone())
}

/// The authors' verdicts, judged once each; the vouching lock taken after the folder's, as
/// everywhere.
fn verdicts(inner: &Inner, state: &FolderState) -> BTreeMap<DeviceId, Verdict> {
    let me = inner.key.id();
    let mut authors = BTreeSet::new();
    for current in state.meta.entries() {
        authors.insert(current.author());
        for conflict in state.meta.open_conflicts(&current.entry) {
            authors.insert(conflict.version.author);
        }
    }
    authors.remove(&me);
    let mut vouching = inner.vouching.lock().unwrap();
    authors
        .into_iter()
        .map(|author| {
            let judged =
                vouching.judge(state.config.id, author, |user| state.members.contains(user));
            let verdict = match judged {
                Ok(()) => Verdict::Keep,
                Err((Unvouched::Revoked, _)) => Verdict::Revoked {
                    lost_ms: vouching.revocation_of(author).and_then(|r| r.lost_ms),
                    learnt_ms: vouching.revoked_since(author),
                    learnt_at: vouching.revoked_at(author),
                },
                Err((Unvouched::NotAMember, _)) => Verdict::TakeOver,
                // Missing here is not enough: a peer must have said it cannot vouch.
                Err((Unvouched::NoCertificate, _)) if vouching.unvouchable(author) => {
                    Verdict::TakeOver
                }
                Err((Unvouched::NoCertificate, _)) => Verdict::Keep,
            };
            (author, verdict)
        })
        .collect()
}

/// Decides, under the folder's lock, what to do with each version of an author no longer
/// vouched for. `exchanged` is when a peer's whole index last came, by the monotonic clock.
fn decide(inner: &Inner, state: &FolderState, exchanged: Option<std::time::Instant>) -> Plan {
    let folder = state.config.id;
    let verdicts = verdicts(inner, state);
    if verdicts.values().all(|v| *v == Verdict::Keep) {
        return Plan::default();
    }
    let mut targets = Vec::new();
    for current in state.meta.entries() {
        let verdict = verdicts
            .get(&current.author())
            .copied()
            .unwrap_or(Verdict::Keep);
        if verdict != Verdict::Keep {
            targets.push(Target {
                entry: current.entry,
                base: state.meta.current_vector(&current.entry),
                path: current.path.to_owned(),
                kind: current.kind.clone(),
                modified_ms: current.modified_ms,
                author: current.author(),
                sequence: current.sequence(),
                verdict,
                conflict: false,
            });
        }
        for conflict in state.meta.open_conflicts(&current.entry) {
            let version = &conflict.version;
            let verdict = verdicts
                .get(&version.author)
                .copied()
                .unwrap_or(Verdict::Keep);
            if verdict != Verdict::Keep {
                targets.push(Target {
                    entry: version.entry,
                    base: version.version.clone(),
                    path: version.path.clone(),
                    kind: version.kind.clone(),
                    modified_ms: version.modified_ms,
                    author: version.author,
                    sequence: conflict.sequence,
                    verdict,
                    conflict: true,
                });
            }
        }
    }

    // When each timed revocation's versions were received: read from disk, not kept in memory
    // (ADR 0035).
    let timed: Vec<u64> = targets
        .iter()
        .filter(|t| {
            matches!(
                t.verdict,
                Verdict::Revoked {
                    lost_ms: Some(_),
                    ..
                }
            )
        })
        .map(|t| t.sequence)
        .collect();
    let (received, receipts_since) = match state.manifests.reader() {
        Some(reader) if !timed.is_empty() => (
            reader.received(&timed).unwrap_or_default(),
            reader.receipts_since().unwrap_or_default(),
        ),
        _ => (Vec::new(), None),
    };
    let received: BTreeMap<u64, Option<i64>> = timed
        .into_iter()
        .zip(received.into_iter().chain(std::iter::repeat(None)))
        .collect();
    let retention = state.meta.recycle_bin().retention();
    let rebuild = state.last_rebuild();
    // By the records; read back before acting (see `take_over`). Content only passing through
    // the block store, as every file of a send-only replica's is, is not held: it is released
    // there. Nor is the same content on disk as something else, such as where a send-only
    // replica left a file another device then moved: a version taken over would be this
    // replica's with no file behind it that its scans follow.
    let keeps = |target: &Target| {
        target.conflict || crate::content::keeps(state, &target.entry, &target.kind)
    };
    let held = |target: &Target| match &target.kind {
        EntryKind::File { content, .. } => {
            keeps(target)
                && state.manifests.get(content).is_some_and(|m| {
                    m.chunks
                        .iter()
                        .all(|c| crate::content::has_chunk(inner, state, &c.hash))
                })
        }
        _ => true,
    };

    let mut plan = Plan::default();
    let mut deletes: BTreeMap<DeviceId, Vec<String>> = BTreeMap::new();
    for target in targets {
        let (lost, learnt, learnt_at) = match target.verdict {
            Verdict::Revoked {
                lost_ms,
                learnt_ms,
                learnt_at,
            } => (lost_ms, learnt_ms, learnt_at),
            _ => (None, None, None),
        };
        // An exchange since this replica learnt of the revocation: a peer holding what this one
        // lacks has acted on it by then.
        let exchanged_since = learnt_at.is_some_and(|l| exchanged.is_some_and(|e| e >= l));
        let revoked = matches!(target.verdict, Verdict::Revoked { .. });
        let received_when = match (lost, learnt) {
            // The time reaches back at most the retention, from when this replica learnt of it,
            // by its own clock: a time the revocation itself states could be anything.
            (Some(lost), Some(learnt)) => {
                let lost = crate::loss::effective_lost(lost, learnt, retention);
                let at = received.get(&target.sequence).copied().flatten();
                Some((crate::loss::judge(at, lost, receipts_since, rebuild), lost))
            }
            _ => None,
        };
        let is_held = held(&target);
        let action = match received_when {
            Some((Received::After, lost)) => {
                if !exchanged_since {
                    plan.waiting = true;
                    continue;
                }
                let kept = (!matches!(target.kind, EntryKind::Tombstone))
                    .then(|| put_back_from(state, &target.entry, target.author, lost))
                    .flatten();
                match kept {
                    Some(kept) => {
                        plan.acts.push((target, Action::PutBack, Some(kept)));
                        continue;
                    }
                    None => Action::Withdraw,
                }
            }
            // No replica may be able to judge: once an exchange has brought any withdrawal
            // another made, what is still the device's is taken over where held, never deleted.
            Some((Received::Unknown, _)) => {
                if !exchanged_since {
                    plan.waiting = true;
                    continue;
                }
                if !is_held {
                    // Content this replica keeps but has not all fetched yet is taken over once
                    // it has: decided again after the fetch.
                    if keeps(&target) {
                        plan.waiting = true;
                    }
                    continue;
                }
                Action::TakeOver
            }
            Some((Received::Before, _)) | None if is_held => Action::TakeOver,
            Some((Received::Before, _)) | None if revoked => {
                if !exchanged_since {
                    plan.waiting = true;
                    continue;
                }
                // In an encrypted folder, content held only by a storage peer is taken over by
                // no one, so a member first tries to fetch it: withdrawn only once a peer has
                // answered and none had it all (ADR 0023).
                if state.config.encryption.is_some() {
                    let unobtainable = match &target.kind {
                        EntryKind::File { content, .. } => inner
                            .unobtainable
                            .lock()
                            .unwrap()
                            .contains(&(state.config.id, *content)),
                        _ => true,
                    };
                    if !unobtainable {
                        plan.waiting = true;
                        continue;
                    }
                }
                Action::Withdraw
            }
            _ => continue,
        };
        if action == Action::Withdraw && !matches!(target.kind, EntryKind::Tombstone) {
            deletes
                .entry(target.author)
                .or_default()
                .push(target.path.clone());
        }
        plan.acts.push((target, action, None));
    }

    // Many withdrawals wait for the user, as a scan's many deletes do (ADR 0037).
    let live = state
        .meta
        .entries()
        .filter(|v| !matches!(v.kind, EntryKind::Tombstone))
        .count();
    let mut held_withdrawals = inner.held_withdrawals.lock().unwrap();
    let mut decided: BTreeMap<DeviceId, Option<bool>> = BTreeMap::new();
    for (author, paths) in &deletes {
        let key = (folder, *author);
        let decision = held_withdrawals.get(&key).and_then(|h| h.withdraw);
        if decision.is_some() || state.config.confirm_deletes.holds(paths.len(), live) {
            decided.insert(*author, decision);
            if decision.is_none() {
                held_withdrawals.insert(
                    key,
                    Held {
                        paths: paths.clone(),
                        withdraw: None,
                    },
                );
            }
        }
    }
    // Decisions acted on are done with, and so are holds with nothing left to withdraw.
    held_withdrawals.retain(|(f, author), held| {
        *f != folder
            || (held.withdraw.is_none() && deletes.contains_key(author))
            || (held.withdraw.is_some() && !decided.contains_key(author))
    });
    drop(held_withdrawals);
    let mut acts = Vec::with_capacity(plan.acts.len());
    for (target, action, kept) in plan.acts {
        match (action, decided.get(&target.author)) {
            // Held: nothing withdrawn until the user says.
            (Action::Withdraw | Action::PutBack, Some(None)) => continue,
            // Kept: taken over where held, as if received before the loss.
            (Action::Withdraw | Action::PutBack, Some(Some(false))) => {
                if held(&target) {
                    acts.push((target, Action::TakeOver, None));
                }
            }
            _ => acts.push((target, action, kept)),
        }
    }
    plan.acts = acts;
    plan
}

/// Takes over, in the folder of `handle`, the current versions and open conflict copies of
/// authors no longer vouched for, and withdraws what a revocation's time of loss calls for (see
/// the module's documentation). Returns how many versions it made.
pub(crate) fn take_over(inner: &Inner, handle: &FolderHandle) -> Result<usize> {
    // A storage peer never takes over or withdraws: members do (ADR 0023).
    if handle.lock().config.is_stored() {
        return Ok(0);
    }
    let mut made = 0;
    loop {
        let exchanged = *handle.exchanged_at.lock().unwrap();
        let (done, waiting) = take_over_once(inner, handle, exchanged)?;
        made += done;
        // An exchange that came while this ran found nothing waiting yet: go again with it.
        if !waiting || *handle.exchanged_at.lock().unwrap() == exchanged {
            return Ok(made);
        }
    }
}

/// One pass of [`take_over`], with `exchanged` when a peer's whole index last came. Returns how
/// many versions it made, and whether something waits.
fn take_over_once(
    inner: &Inner,
    handle: &FolderHandle,
    exchanged: Option<std::time::Instant>,
) -> Result<(usize, bool)> {
    let (folder, plan) = {
        let state = handle.lock();
        // A folder being rebuilt is decided once it is done: its records are incomplete.
        if state.rebuilding() {
            inner
                .takeover_waiting
                .lock()
                .unwrap()
                .insert(state.config.id);
            return Ok((0, false));
        }
        (state.config.id, decide(inner, &state, exchanged))
    };
    let mut waiting = plan.waiting;
    if plan.acts.is_empty() {
        if waiting {
            inner.takeover_waiting.lock().unwrap().insert(folder);
        }
        return Ok((0, waiting));
    }

    // Content to take over is read back whole, with the folder's lock let go.
    let mut to_read: BTreeMap<ContentHash, Vec<tidefell_proto::ChunkHash>> = BTreeMap::new();
    {
        let state = handle.lock();
        for (target, action, _) in &plan.acts {
            if let (Action::TakeOver, EntryKind::File { content, .. }) = (action, &target.kind) {
                if let Some(manifest) = state.manifests.get(content) {
                    to_read.insert(*content, manifest.chunks.iter().map(|c| c.hash).collect());
                }
            }
        }
    }
    let readable: BTreeSet<ContentHash> = to_read
        .into_iter()
        .filter(|(_, chunks)| {
            chunks
                .iter()
                .all(|hash| crate::content::read_chunk(inner, handle, hash).is_some())
        })
        .map(|(content, _)| content)
        .collect();

    let mut made = 0;
    let mut put_backs = Vec::new();
    {
        let mut state = handle.lock();
        for (target, action, kept) in plan.acts {
            // Changed meanwhile: decided again, as what waits is.
            let still = if state.meta.current_vector(&target.entry) == target.base {
                state
                    .meta
                    .current(&target.entry)
                    .is_some_and(|c| c.author() == target.author)
            } else {
                state
                    .meta
                    .open_conflicts(&target.entry)
                    .any(|c| c.version.version == target.base && c.version.author == target.author)
            };
            if !still {
                waiting = true;
                continue;
            }
            let (kind, modified_ms) = match action {
                Action::TakeOver => {
                    let readable = match &target.kind {
                        EntryKind::File { content, .. } => readable.contains(content),
                        _ => true,
                    };
                    if !readable {
                        waiting = true;
                        continue;
                    }
                    (target.kind, target.modified_ms)
                }
                Action::Withdraw => (EntryKind::Tombstone, inner.now_ms()),
                Action::PutBack => {
                    if let Some(previous) = kept {
                        put_backs.push(PutBack {
                            entry: target.entry,
                            base: target.base,
                            path: target.path,
                            previous,
                        });
                    }
                    continue;
                }
            };
            let signer = match crate::encrypted::signer(inner, &mut state, &kind, None) {
                Ok(signer) => signer,
                Err(e) => {
                    warn!(%folder, path = %target.path, error = %e, "taking over a version failed");
                    continue;
                }
            };
            match state.meta.local_change_from(
                &signer,
                target.entry,
                &target.base,
                &target.path,
                kind,
                modified_ms,
            ) {
                Ok(_) => made += 1,
                Err(e) => {
                    warn!(%folder, path = %target.path, error = %e, "taking over a version failed")
                }
            }
        }
        if made > 0 {
            // Files on disk as they were now correspond to this device's versions.
            state.catch_up_bases();
            state.save()?;
            info!(%folder, versions = made, "took over or withdrew versions of authors no longer vouched for");
        }
    }
    if waiting {
        inner.takeover_waiting.lock().unwrap().insert(folder);
    }
    if made > 0 {
        inner.changes.send_modify(|n| *n += 1);
    }
    for put_back in put_backs {
        match put_back_one(inner, handle, put_back) {
            Ok(true) => made += 1,
            Ok(false) => {}
            Err(e) => warn!(error = %e, "putting back a version failed"),
        }
    }
    Ok((made, waiting))
}

/// Puts back the content kept in `put_back.previous` as a change of this device's, building on
/// the version it withdraws. Its chunks go to the block store first, so this device can write the
/// file and serve it; they are kept from compaction from before they are stored, by their
/// manifest and the content being marked fetched, as a download's are: otherwise a compaction
/// between storing them and recording the version could remove them, leaving a version no
/// device can supply. Returns false if the entry changed meanwhile: the next takeover decides
/// again.
fn put_back_one(inner: &Inner, handle: &FolderHandle, put_back: PutBack) -> Result<bool> {
    let PutBack {
        entry,
        base,
        path,
        previous,
    } = put_back;
    let root = handle.lock().config.path.clone();
    let file = crate::history::history_dir(&root).join(&previous.file);
    let read = |sink: &mut dyn FnMut(tidefell_proto::ChunkHash, &[u8]) -> std::io::Result<()>| {
        let reader =
            std::io::BufReader::new(std::fs::File::open(&file).map_err(crate::io_err(&file))?);
        tidefell_chunker::chunk_reader(reader, tidefell_chunker::Params::DEFAULT, |chunk, data| {
            sink(chunk.hash, data)
        })
        .map_err(crate::io_err(&file))
    };
    // What the kept file holds, before anything is stored.
    let manifest = read(&mut |_, _| Ok(()))?;
    if manifest.content != previous.content {
        warn!(path = %path, "a kept version no longer holds what it did; not put back");
        return Ok(false);
    }
    let changed = |state: &FolderState| {
        state.meta.current_vector(&entry) != base
            && !state
                .meta
                .open_conflicts(&entry)
                .any(|c| c.version.version == base)
    };
    {
        let mut state = handle.lock();
        if changed(&state) {
            return Ok(false);
        }
        state.add_manifest(manifest);
        inner.fetching.lock().unwrap().insert(previous.content);
    }
    let _kept = crate::materialize::Fetching(inner, previous.content);
    let stored = read(&mut |hash, data| {
        inner
            .store
            .lock()
            .unwrap()
            .put_verified(hash, data)
            .map_err(std::io::Error::other)
    })?;
    if stored.content != previous.content {
        warn!(path = %path, "a kept version changed while it was put back; not put back");
        return Ok(false);
    }
    let mut state = handle.lock();
    if changed(&state) {
        return Ok(false);
    }
    let kind = EntryKind::File {
        content: previous.content,
        size: previous.size,
        executable: previous.executable,
    };
    let signer = crate::encrypted::signer(inner, &mut state, &kind, None)?;
    state
        .meta
        .local_change_from(&signer, entry, &base, &path, kind, previous.modified_ms)?;
    state.save()?;
    info!(folder = %state.config.id, path = %path, "put back the version from before the loss");
    drop(state);
    inner.changes.send_modify(|n| *n += 1);
    Ok(true)
}
