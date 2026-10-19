//! Encrypted folders on member devices (ADR 0023, "On member devices"; `specs/sync-v2.md`).
//!
//! The folder's versions in `meta` are decrypted; what each came as, a signed sealed version, is
//! kept beside it. Besides versions, the folder's index holds the members' encryption keys, the
//! grants of folder keys, and sealed versions this device cannot open yet, as [`FolderState`]'s
//! `items`, under sequences from the same counter. Folder keys this device holds are in `keys`.
//!
//! Locks: a folder's before the vouching lock, as everywhere.

use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;
use tidefell_crypto::{open_grant, seal_grant, EncryptionSecret, FolderKey, GrantFor};
use tidefell_meta::{Seal, Verified, Wire};
use tidefell_net::ItemPage;
use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::sync2::{
    ChunkList, EncryptionKey, Item, KeyGrant, SealedPayload, SealedVersion,
};
use tidefell_proto::{Causality, ChunkHash, ChunkId, DeviceId, KeyId, Signed};
use tracing::{debug, info, warn};

use crate::engine::{FolderHandle, Inner};
use crate::state::FolderState;
use crate::vouch::Unvouched;
use crate::{Error, Result};

/// The folder key `id`, if this device holds it.
pub(crate) fn key(state: &FolderState, id: &KeyId) -> Option<FolderKey> {
    state
        .keys
        .get(id)
        .map(|(generation, key)| FolderKey::new(*key, *generation))
}

/// The key new content is sealed with: the newest generation held, and of that generation the
/// key with the lower key ID (`specs/encryption-v1.md`).
pub(crate) fn newest_key(state: &FolderState) -> Option<FolderKey> {
    state
        .keys
        .iter()
        .max_by(|(a_id, (a_gen, _)), (b_id, (b_gen, _))| a_gen.cmp(b_gen).then(b_id.cmp(a_id)))
        .map(|(_, (generation, key))| FolderKey::new(*key, *generation))
}

fn hold(state: &mut FolderState, key: &FolderKey) {
    state
        .keys
        .insert(key.id(), (key.generation(), *key.as_bytes()));
}

/// How a local change is made into what is sent: signed, or sealed and signed.
pub(crate) enum Signer {
    Plain(Box<SigningKey>),
    Sealed(Box<Sealer>),
}

impl Seal for Signer {
    fn device(&self) -> DeviceId {
        match self {
            Signer::Plain(key) => key.device(),
            Signer::Sealed(sealer) => sealer.signing.device(),
        }
    }

    fn seal(&self, version: &EntryVersion) -> tidefell_meta::Result<Wire> {
        match self {
            Signer::Plain(key) => key.seal(version),
            Signer::Sealed(sealer) => sealer.seal(version),
        }
    }
}

/// Seals local changes with the folder's newest key: the version's content is the chunks
/// given, which must be the version's.
pub(crate) struct Sealer {
    signing: SigningKey,
    key: FolderKey,
    /// The content's chunk IDs as the version lists them: inline, or in list blobs, which
    /// [`signer`] recorded so this device can serve them.
    chunks: ChunkList,
    lens: Vec<u32>,
}

impl Sealer {
    fn seal(&self, version: &EntryVersion) -> tidefell_meta::Result<Wire> {
        let file = matches!(version.kind, EntryKind::File { .. });
        let (chunks, lens) = if file {
            (self.chunks.clone(), self.lens.clone())
        } else {
            (ChunkList::Inline(Vec::new()), Vec::new())
        };
        let payload = SealedPayload {
            path: version.path.clone(),
            kind: version.kind.clone(),
            modified_ms: version.modified_ms,
            chunk_lens: lens,
        };
        let sealed = self
            .key
            .seal_payload(
                &version.folder,
                &version.entry,
                &version.author,
                &payload.encode(),
            )
            .map_err(|e| tidefell_meta::Error::Sealing(e.to_string()))?;
        let sealed = SealedVersion {
            folder: version.folder,
            entry: version.entry,
            author: version.author,
            generation: self.key.generation(),
            key: self.key.id(),
            chunks,
            sealed,
            version: version.version.clone(),
        };
        Ok(Wire::Sealed(Signed::sign(&sealed, &self.signing)))
    }
}

/// The signer for a local change of `kind` in this folder: the device key for a plaintext
/// folder; for an encrypted one, a sealer with the newest key and the content's chunk IDs,
/// recorded as they are found. `file` is where the content is on disk if the records do not
/// say yet, as for a file the scan is recording.
pub(crate) fn signer(
    inner: &Inner,
    state: &mut FolderState,
    kind: &EntryKind,
    file: Option<&str>,
) -> Result<Signer> {
    let signing = inner.key.signing_key().clone();
    if state.config.encryption.is_none() {
        return Ok(Signer::Plain(Box::new(signing)));
    }
    let key = newest_key(state).ok_or_else(|| {
        Error::Invalid(format!(
            "folder {}: no key yet; waiting for a member device to grant one",
            state.config.id
        ))
    })?;
    let (chunks, lens) = match kind {
        EntryKind::File { content, .. } => {
            let manifest = state
                .manifests
                .get(content)
                .ok_or_else(|| Error::Invalid(format!("content {content} has no manifest")))?;
            let mut ids = Vec::with_capacity(manifest.chunks.len());
            for chunk in &manifest.chunks {
                let also = file.map(|f| (f, content));
                ids.push(chunk_id(inner, state, &key, &chunk.hash, also)?);
            }
            let lens: Vec<u32> = manifest.chunks.iter().map(|c| c.len).collect();
            state.sealed_chunks.insert(
                (*content, key.id()),
                ids.iter().copied().zip(lens.iter().copied()).collect(),
            );
            // A file of more than 4096 chunks lists them in list blobs (encryption-v1.md).
            let (list, blobs) = tidefell_proto::sync2::chunk_list(&ids).ok_or_else(|| {
                Error::Invalid(format!("content {content} has too many chunks to list"))
            })?;
            for (id, bytes) in blobs {
                state.list_blobs.insert(id, bytes);
            }
            (list, lens)
        }
        _ => (ChunkList::Inline(Vec::new()), Vec::new()),
    };
    Ok(Signer::Sealed(Box::new(Sealer {
        signing,
        key,
        chunks,
        lens,
    })))
}

/// The ID of chunk `hash` under `key`, reading the chunk if it is not recorded yet.
fn chunk_id(
    inner: &Inner,
    state: &mut FolderState,
    key: &FolderKey,
    hash: &ChunkHash,
    also: Option<(&str, &tidefell_proto::ContentHash)>,
) -> Result<ChunkId> {
    if let Some(id) = find_chunk_id(state, hash, key.id()) {
        return Ok(id);
    }
    let data = crate::content::read_chunk_here(inner, state, hash, also)
        .ok_or_else(|| Error::Invalid(format!("chunk {hash} is not here to seal")))?;
    let id = key.chunk_id(&data);
    record_chunk_id(state, id, *hash, key.id());
    Ok(id)
}

fn add_item(state: &mut FolderState, item: Item) -> u64 {
    let sequence = state.meta.next_sequence();
    if let Some(digests) = state.item_digests.as_mut() {
        digests.insert(digest(&item));
    }
    state.items.insert(sequence, item);
    sequence
}

fn digest(item: &Item) -> [u8; 32] {
    *blake3::hash(&postcard::to_stdvec(item).unwrap_or_default()).as_bytes()
}

/// Whether `item` is kept already, by its digest.
fn has_item(state: &mut FolderState, item: &Item) -> bool {
    if state.item_digests.is_none() {
        state.item_digests = Some(state.items.values().map(digest).collect());
    }
    state
        .item_digests
        .as_ref()
        .is_some_and(|digests| digests.contains(&digest(item)))
}

/// Removes the item at `sequence`.
fn remove_item(state: &mut FolderState, sequence: &u64) {
    if let Some(item) = state.items.remove(sequence) {
        if let Some(digests) = state.item_digests.as_mut() {
            digests.remove(&digest(&item));
        }
    }
}

/// The ID of chunk `hash` under `key`, if recorded.
fn find_chunk_id(state: &mut FolderState, hash: &ChunkHash, key: KeyId) -> Option<ChunkId> {
    let index = state.chunk_index.get_or_insert_with(|| {
        state
            .chunk_ids
            .iter()
            .map(|(id, (hash, key))| ((*hash, *key), *id))
            .collect()
    });
    index.get(&(*hash, key)).copied()
}

/// Records that chunk `id` is `hash` under `key`.
pub(crate) fn record_chunk_id(state: &mut FolderState, id: ChunkId, hash: ChunkHash, key: KeyId) {
    if let Some(index) = state.chunk_index.as_mut() {
        index.insert((hash, key), id);
    }
    state.chunk_ids.insert(id, (hash, key));
}

/// On a storage peer, keeps a sealed version whose author it vouches for (ADR 0023, "On
/// storage peers"): served while no kept version of its entry supersedes it by vector. One it
/// supersedes stops being served and waits out the folder's retention; one that arrives
/// superseded is kept the same way. Returns true if it was kept.
fn keep_stored(
    state: &mut FolderState,
    sealed: &SealedVersion,
    signed: Signed<SealedVersion>,
    now: i64,
) -> bool {
    let mut heads = state
        .stored_heads
        .take()
        .unwrap_or_else(|| stored_heads(state));
    let entry = heads.entry(sealed.entry).or_default();
    let compared: Vec<Causality> = entry
        .iter()
        .map(|(_, vector)| sealed.version.compare(vector))
        .collect();
    // The same vector is the same version: kept already.
    let kept = if compared.contains(&Causality::Equal) {
        false
    } else {
        let sequence = add_item(state, Item::Version(signed));
        if compared.contains(&Causality::Before) {
            state.superseded.insert(sequence, now);
        } else {
            let mut index = 0;
            entry.retain(|(old, _)| {
                let superseded = compared.get(index) == Some(&Causality::After);
                index += 1;
                if superseded {
                    state.superseded.insert(*old, now);
                }
                !superseded
            });
            entry.push((sequence, sealed.version.clone()));
        }
        true
    };
    state.stored_heads = Some(heads);
    kept
}

/// The kept sealed versions no other supersedes, by entry, read from `items`: their bodies
/// were verified when kept.
fn stored_heads(
    state: &FolderState,
) -> std::collections::HashMap<tidefell_proto::EntryId, Vec<(u64, tidefell_proto::VersionVector)>> {
    use tidefell_proto::SignedKind as _;
    let mut heads: std::collections::HashMap<_, Vec<_>> = std::collections::HashMap::new();
    for (sequence, item) in state.items.iter() {
        let Item::Version(signed) = item else {
            continue;
        };
        if state.superseded.contains_key(sequence) {
            continue;
        }
        if let Ok(sealed) = SealedVersion::decode(&signed.body) {
            heads
                .entry(sealed.entry)
                .or_default()
                .push((*sequence, sealed.version));
        }
    }
    heads
}

/// What a storage peer stores of a folder.
pub(crate) fn stored_info(
    state: &FolderState,
    stored: crate::config::Stored,
) -> crate::views::StoredInfo {
    let superseded = state.superseded.len();
    let versions = state
        .items
        .values()
        .filter(|item| matches!(item, Item::Version(_)))
        .count();
    crate::views::StoredInfo {
        versions: versions.saturating_sub(superseded),
        superseded,
        quota_bytes: stored.quota_bytes,
        bytes: state.sealed_store.as_ref().map_or(0, |s| s.bytes()),
        missing_chunks: crate::stored::missing(state),
    }
}

/// Drops, on a storage peer, the superseded sealed versions whose time in the folder's
/// retention is over (ADR 0023). Returns how many it dropped.
pub(crate) fn purge_superseded(state: &mut FolderState, now: i64) -> usize {
    const DAY_MS: i64 = 24 * 60 * 60 * 1000;
    let Some(days) = state.config.retention.days() else {
        return 0;
    };
    let keep_ms = i64::from(days).saturating_mul(DAY_MS);
    let due: Vec<u64> = state
        .superseded
        .iter()
        .filter(|(_, at)| at.saturating_add(keep_ms) <= now)
        .map(|(sequence, _)| *sequence)
        .collect();
    for sequence in &due {
        state.superseded.remove(sequence);
        remove_item(state, sequence);
    }
    due.len()
}

fn signer_of(item: &Item) -> DeviceId {
    let signer = match item {
        Item::Version(signed) => signed.signer,
        Item::EncryptionKey(signed) => signed.signer,
        Item::Grant(signed) => signed.signer,
    };
    DeviceId(*signer.as_bytes())
}

/// Readies an encrypted folder once it is open: makes a first key if this device holds none,
/// publishes its encryption key, and grants the keys it holds to devices waiting for them.
///
/// Every member device that holds no key makes one, so a folder made on several devices at
/// once, or joined before any grant reaches it, has several keys of generation 1: each is
/// granted to every member, and new content uses the one with the lower key ID (ADR 0023).
pub(crate) fn prepare(inner: &Inner, handle: &FolderHandle) {
    let mut state = handle.lock();
    // A storage peer neither makes nor asks for keys (ADR 0023).
    if state.config.encryption.is_none() || state.config.is_stored() {
        return;
    }
    if state.keys.is_empty() {
        match FolderKey::generate(1) {
            Ok(key) => hold(&mut state, &key),
            Err(e) => warn!(folder = %state.config.id, error = %e, "making a folder key failed"),
        }
    }
    publish_key(inner, &mut state);
    grant_pending(inner, &mut state);
    if let Err(e) = state.save() {
        warn!(folder = %state.config.id, error = %e, "saving an encrypted folder's keys failed");
    }
}

/// Publishes this device's encryption key in the folder, unless it is there.
pub(crate) fn publish_key(inner: &Inner, state: &mut FolderState) {
    let me = inner.key.id();
    let public = inner.encryption.public();
    let published = state.items.values().any(|item| {
        matches!(item, Item::EncryptionKey(k)
            if k.verify_own().is_ok_and(|k| k.device == me && k.key == public))
    });
    if !published {
        let key = EncryptionKey {
            device: me,
            key: public,
            issued_ms: inner.now_ms(),
        };
        add_item(
            state,
            Item::EncryptionKey(Signed::sign(&key, inner.key.signing_key())),
        );
    }
}

fn grant(
    inner: &Inner,
    state: &mut FolderState,
    key: &FolderKey,
    recipient: DeviceId,
    recipient_key: [u8; 32],
) -> Result<()> {
    let to = GrantFor {
        folder: state.config.id,
        recipient,
        recipient_key,
    };
    let (enc, sealed) = seal_grant(key, &to).map_err(|e| Error::Invalid(e.to_string()))?;
    let grant = KeyGrant {
        folder: state.config.id,
        generation: key.generation(),
        key: key.id(),
        recipient,
        recipient_key,
        enc,
        sealed,
    };
    add_item(
        state,
        Item::Grant(Signed::sign(&grant, inner.key.signing_key())),
    );
    Ok(())
}

/// The newest encryption key each device published here.
fn encryption_keys(state: &FolderState) -> BTreeMap<DeviceId, EncryptionKey> {
    let mut newest: BTreeMap<DeviceId, EncryptionKey> = BTreeMap::new();
    for item in state.items.values() {
        if let Item::EncryptionKey(signed) = item {
            if let Ok(key) = signed.verify_own() {
                let newer = newest
                    .get(&key.device)
                    .is_none_or(|old| key.issued_ms > old.issued_ms);
                if newer {
                    newest.insert(key.device, key);
                }
            }
        }
    }
    newest
}

/// Makes the next generation of the folder key if a key of the newest generation this device
/// holds was granted to a device that is revoked, or whose user is no longer a member
/// (`specs/sync-v2.md`). Once the new key is granted only to the devices that remain, no grant
/// of the newest generation leaks, so a removal rotates once, or twice if two devices make it at
/// the same moment. Returns true if it made a key.
pub(crate) fn rotate_if_needed(inner: &Inner, state: &mut FolderState) -> bool {
    let Some(newest) = newest_key(state) else {
        return false;
    };
    let generation = newest.generation();
    let newest_ids: Vec<KeyId> = state
        .keys
        .iter()
        .filter(|(_, (g, _))| *g == generation)
        .map(|(id, _)| *id)
        .collect();
    let (folder, me) = (state.config.id, inner.key.id());
    let members = state.members.clone();
    let leaked = {
        let mut vouching = inner.vouching.lock().unwrap();
        state.items.values().any(|item| {
            let Item::Grant(signed) = item else {
                return false;
            };
            signed.verify().is_ok_and(|grant| {
                newest_ids.contains(&grant.key)
                    && grant.recipient != me
                    // A device not known here is not judged gone: only one shown revoked or of
                    // a user no longer a member is.
                    && matches!(
                        vouching.judge(folder, grant.recipient, |user| members.contains(user)),
                        Err((Unvouched::Revoked | Unvouched::NotAMember, _))
                    )
            })
        })
    };
    if !leaked {
        return false;
    }
    match FolderKey::generate(generation + 1) {
        Ok(key) => {
            hold(state, &key);
            info!(%folder, generation = generation + 1, "rotated the folder key");
            true
        }
        Err(e) => {
            warn!(%folder, error = %e, "making the next folder key failed");
            false
        }
    }
}

/// Grants every key this device holds to each device vouched for that published an encryption
/// key and holds no grant of it to that key (`specs/sync-v2.md`), after rotating if a removal or
/// revocation calls for it. Returns how many grants it made.
pub(crate) fn grant_pending(inner: &Inner, state: &mut FolderState) -> usize {
    if state.config.encryption.is_none() || state.config.is_stored() || state.keys.is_empty() {
        return 0;
    }
    let rotated = rotate_if_needed(inner, state);
    let folder = state.config.id;
    let members = state.members.clone();
    let me = inner.key.id();
    let mut vouching = inner.vouching.lock().unwrap();
    // A grant counts only while its signer is vouched for: the recipient drops one from a
    // revoked device, so another holder of the key grants it again.
    let granted: Vec<(KeyId, DeviceId, [u8; 32])> = state
        .items
        .values()
        .filter(|item| {
            let signer = signer_of(item);
            signer == me
                || vouching
                    .judge(folder, signer, |user| members.contains(user))
                    .is_ok()
        })
        .filter_map(|item| match item {
            Item::Grant(g) => g
                .verify()
                .ok()
                .map(|g| (g.key, g.recipient, g.recipient_key)),
            _ => None,
        })
        .collect();
    let recipients: Vec<EncryptionKey> = {
        encryption_keys(state)
            .into_values()
            .filter(|key| {
                key.device == me
                    || vouching
                        .judge(folder, key.device, |user| members.contains(user))
                        .is_ok()
            })
            .collect()
    };
    drop(vouching);
    let keys: Vec<FolderKey> = state
        .keys
        .iter()
        .map(|(_, (generation, key))| FolderKey::new(*key, *generation))
        .collect();
    let mut made = 0;
    for key in &keys {
        for recipient in &recipients {
            let has = granted
                .iter()
                .any(|(k, d, r)| *k == key.id() && *d == recipient.device && *r == recipient.key);
            if !has {
                match grant(inner, state, key, recipient.device, recipient.key) {
                    Ok(()) => made += 1,
                    Err(e) => warn!(%folder, error = %e, "granting a folder key failed"),
                }
            }
        }
    }
    if made > 0 {
        debug!(%folder, grants = made, "granted folder keys");
    }
    // A key made and granted to no one yet is still saved: the caller saves when this is not 0.
    made + usize::from(rotated)
}

/// A page of the folder's index for sync/2: its versions, as sealed, and its other items,
/// merged by sequence, committed ones only.
pub(crate) fn index_since(
    state: &FolderState,
    epoch: u64,
    since: u64,
    limit: usize,
) -> Option<ItemPage> {
    let committed = state.committed();
    let since = if epoch == state.meta.epoch() {
        since
    } else {
        0
    };
    let (versions, meta_up_to) = state.meta.index_since(epoch, since, limit);
    enum Source {
        Version,
        Item,
    }
    let mut merged: Vec<(u64, Source)> = versions
        .iter()
        .map(|v| (v.sequence, Source::Version))
        .chain(
            state
                .items
                .range(since.saturating_add(1)..)
                // A storage peer serves only what no kept version supersedes (ADR 0023).
                .filter(|(s, _)| !state.superseded.contains_key(s))
                .take(limit)
                .map(|(s, _)| (*s, Source::Item)),
        )
        .collect();
    merged.sort_by_key(|(s, _)| *s);
    let truncated = merged.len() >= limit;
    merged.truncate(limit);
    let mut up_to = if truncated {
        merged.last().map_or(since, |(s, _)| *s)
    } else {
        meta_up_to.max(state.items.keys().next_back().copied().unwrap_or(0))
    };
    // Only what is on disk, so a peer is never told of what a crash could lose (ADR 0034).
    merged.retain(|(s, _)| *s <= committed);
    up_to = up_to.min(committed);
    let version_sequences: Vec<u64> = merged
        .iter()
        .filter(|(_, source)| matches!(source, Source::Version))
        .map(|(s, _)| *s)
        .collect();
    let sealed = match state.manifests.reader() {
        Some(reader) => reader.sealed_versions(&version_sequences).ok()?,
        None => Vec::new(),
    };
    let mut sealed = sealed.into_iter();
    let items = merged
        .into_iter()
        .filter_map(|(sequence, source)| match source {
            Source::Version => sealed.next().map(Item::Version),
            Source::Item => state.items.get(&sequence).cloned(),
        })
        .collect();
    Some(ItemPage {
        epoch: state.meta.epoch(),
        up_to,
        items,
    })
}

/// What applying items did.
#[derive(Debug, Default)]
pub(crate) struct Applied {
    pub(crate) changed: bool,
    /// A key arrived: versions waiting for it can be opened, and keys granted on.
    pub(crate) keys: bool,
    /// An entry these touched has a current version or open conflict copy by an author no
    /// longer vouched for, as one concurrent with a put-back does: a takeover is due (ADR 0040).
    pub(crate) takeover: bool,
}

/// Applies items `peer` sent: versions opened with a key held and applied, or kept for later;
/// encryption keys and grants kept; grants to this device opened. Each item's signer must be
/// vouched for.
pub(crate) fn apply(
    inner: &Inner,
    handle: &FolderHandle,
    peer: DeviceId,
    items: Vec<Item>,
) -> Applied {
    let mut applied = Applied::default();
    let mut touched = Vec::new();
    let mut state = handle.lock();
    let folder = state.config.id;
    let now = inner.now_ms();
    // A version that waited on a storage peer arrives late without being late, so no time is
    // recorded for it (ADR 0023): it is never withdrawn for a time of loss.
    let from_storage_peer = is_storage_peer(inner, &state, peer);
    let rebuilding = state.rebuilding();
    state
        .meta
        .set_record_receipts(!rebuilding && !from_storage_peer);
    for item in items {
        // Sent again, as after a reconnection: already kept.
        if has_item(&mut state, &item) {
            continue;
        }
        let signer = signer_of(&item);
        if signer != inner.key.id() {
            let members = state.members.clone();
            let judged = inner
                .vouching
                .lock()
                .unwrap()
                .judge(folder, signer, |user| members.contains(user));
            if judged.is_err() {
                continue;
            }
        }
        match item {
            Item::Version(signed) => {
                applied.changed |= apply_version(&mut state, signed, now, &mut touched);
            }
            Item::EncryptionKey(signed) => {
                let Ok(key) = signed.verify_own() else {
                    continue;
                };
                let known = state.items.values().any(|i| {
                    matches!(i, Item::EncryptionKey(k)
                        if k.verify_own().is_ok_and(|k| k.device == key.device
                            && k.issued_ms >= key.issued_ms))
                });
                if !known {
                    // An older key of the device is no longer in the index.
                    state.items.retain(|_, i| {
                        !matches!(i, Item::EncryptionKey(k)
                            if k.verify_own().is_ok_and(|k| k.device == key.device))
                    });
                    // Rare: the digests are made again when next needed.
                    state.item_digests = None;
                    add_item(&mut state, Item::EncryptionKey(signed));
                    applied.changed = true;
                }
            }
            Item::Grant(signed) => {
                let Ok(grant) = signed.verify() else {
                    continue;
                };
                if grant.folder != folder || has_item(&mut state, &Item::Grant(signed.clone())) {
                    continue;
                }
                add_item(&mut state, Item::Grant(signed));
                applied.changed = true;
                if open_own_grant(inner, &mut state, &grant) {
                    applied.keys = true;
                }
            }
        }
    }
    if applied.keys {
        // When these arrived is not when they are opened, and what sent them is not known
        // here: no arrival time is recorded, so a time of loss never withdraws them (ADR 0040).
        state.meta.set_record_receipts(false);
        applied.changed |= open_waiting(&mut state, now, &mut touched);
    }
    state.meta.set_record_receipts(!rebuilding);
    let me = inner.key.id();
    let members = state.members.clone();
    let mut vouching = inner.vouching.lock().unwrap();
    applied.takeover = touched.iter().any(|entry| {
        let current = state.meta.current(entry).map(|c| c.author());
        let conflicts = state.meta.open_conflicts(entry).map(|c| c.version.author);
        current.into_iter().chain(conflicts).any(|author| {
            author != me
                && vouching
                    .judge(folder, author, |user| members.contains(user))
                    .is_err()
        })
    });
    drop(vouching);
    applied
}

/// Whether `peer` is a storage peer of the folder: a device whose user is not a member.
pub(crate) fn is_storage_peer(inner: &Inner, state: &FolderState, peer: DeviceId) -> bool {
    let vouching = inner.vouching.lock().unwrap();
    vouching
        .certificate(peer)
        .and_then(|c| c.verify_certificate().ok())
        .is_some_and(|c| !state.members.contains(&c.user))
}

/// Opens and applies a sealed version if a key is held, or keeps it to open later. Returns true
/// if anything changed.
fn apply_version(
    state: &mut FolderState,
    signed: Signed<SealedVersion>,
    now: i64,
    touched: &mut Vec<tidefell_proto::EntryId>,
) -> bool {
    let Ok(sealed) = signed.verify_authored() else {
        return false;
    };
    touched.push(sealed.entry);
    if sealed.folder != state.config.id || !sealed.version.within_ceiling() {
        return false;
    }
    if state.config.is_stored() {
        return keep_stored(state, &sealed, signed, now);
    }
    match open(state, &sealed, signed.clone()) {
        Some(Ok((verified, content))) => apply_opened(state, verified, content, now),
        // Opened, and wrong: dropped as a version that fails its checks.
        Some(Err(e)) => {
            debug!(folder = %state.config.id, error = %e, "dropped a sealed version");
            false
        }
        None => keep_waiting(state, &sealed, signed),
    }
}

/// The decrypted version of `sealed`, checked, if this device holds its key; `None` if it does
/// not yet, or if it lists its chunks in list blobs, which are not fetched yet.
fn open(
    state: &FolderState,
    sealed: &SealedVersion,
    signed: Signed<SealedVersion>,
) -> Option<std::result::Result<(Verified, Option<SealedContent>), String>> {
    let key = key(state, &sealed.key)?;
    // A version listing its chunks in list blobs opens once they are all here.
    let ids = &chunk_ids_of(state, sealed)?;
    Some((|| {
        let payload = key
            .open_payload(
                &sealed.folder,
                &sealed.entry,
                &sealed.author,
                &sealed.sealed,
            )
            .map_err(|e| e.to_string())?;
        let payload = SealedPayload::decode(&payload).map_err(|e| e.to_string())?;
        let lens = payload.chunk_lens.clone();
        let version = payload
            .into_version(sealed, ids.len())
            .map_err(|e| e.to_string())?;
        let content = match version.kind {
            EntryKind::File { content, .. } => {
                Some((content, (key.id(), ids.iter().copied().zip(lens).collect())))
            }
            _ => None,
        };
        let verified =
            Verified::sealed(state.config.id, version, signed).map_err(|e| e.to_string())?;
        Ok((verified, content))
    })())
}

/// The chunk IDs `sealed` lists, inline or in list blobs; `None` while a list blob it names is
/// not here.
pub(crate) fn chunk_ids_of(state: &FolderState, sealed: &SealedVersion) -> Option<Vec<ChunkId>> {
    match &sealed.chunks {
        ChunkList::Inline(ids) => Some(ids.clone()),
        ChunkList::Lists(lists) => {
            let mut ids = Vec::new();
            for list in lists {
                let bytes = state.list_blobs.get(list)?;
                ids.extend(tidefell_proto::sync2::read_list_blob(list, bytes)?);
            }
            Some(ids)
        }
    }
}

/// Fetches the list blobs that kept sealed versions name and this device lacks, from the peers
/// sharing the folder, and opens the versions that waited for them. Returns true if it fetched
/// any.
pub(crate) async fn fetch_lists(
    inner: &std::sync::Arc<Inner>,
    node: &std::sync::Arc<tidefell_net::Node>,
    handle: &FolderHandle,
) -> bool {
    use tidefell_net::{BlobKind, BlobResult};
    use tidefell_proto::blobs2::BlobId;
    use tidefell_proto::SignedKind as _;

    let (folder, mut wanted) = {
        let state = handle.lock();
        let mut wanted: Vec<tidefell_proto::ListId> = Vec::new();
        for item in state.items.values() {
            let Item::Version(signed) = item else {
                continue;
            };
            let Ok(sealed) = SealedVersion::decode(&signed.body) else {
                continue;
            };
            if let ChunkList::Lists(lists) = sealed.chunks {
                for list in lists {
                    if !state.list_blobs.contains_key(&list) && !wanted.contains(&list) {
                        wanted.push(list);
                    }
                }
            }
        }
        (state.config.id, wanted)
    };
    if wanted.is_empty() {
        return false;
    }
    let peers: Vec<DeviceId> = node
        .encrypted_peers()
        .into_iter()
        .filter(|p| p.shared_folders().contains(&folder))
        .map(|p| p.device())
        .collect();
    let mut fetched = false;
    for peer in peers {
        if wanted.is_empty() {
            break;
        }
        let Ok(endpoint) = tidefell_net::EndpointId::from_bytes(peer.as_bytes()) else {
            continue;
        };
        for batch in wanted
            .clone()
            .chunks(tidefell_proto::blobs2::MAX_BLOBS_PER_REQUEST)
        {
            let request: Vec<(BlobId, BlobKind)> = batch
                .iter()
                .map(|id| (BlobId::from(*id), BlobKind::List))
                .collect();
            let Ok(results) = node.fetch_blobs(endpoint, folder, &request).await else {
                break;
            };
            let mut state = handle.lock();
            for result in results {
                // The network layer checked it against its ID.
                let BlobResult::Found(id, bytes) = result else {
                    continue;
                };
                let list = tidefell_proto::ListId(id.0);
                if batch.contains(&list) {
                    state.list_blobs.insert(list, bytes);
                    wanted.retain(|w| *w != list);
                    fetched = true;
                }
            }
        }
    }
    if fetched {
        let mut state = handle.lock();
        let now = inner.now_ms();
        let mut touched = Vec::new();
        if !state.config.is_stored() {
            state.meta.set_record_receipts(false);
            open_waiting(&mut state, now, &mut touched);
            let rebuilding = state.rebuilding();
            state.meta.set_record_receipts(!rebuilding);
        }
        if let Err(e) = state.save() {
            warn!(%folder, error = %e, "saving fetched list blobs failed");
        }
        drop(state);
        inner.changed();
    }
    fetched
}

/// A file's content as a sealed version names it: its hash, and its chunks' key, IDs and
/// lengths.
type SealedContent = (tidefell_proto::ContentHash, (KeyId, Vec<(ChunkId, u32)>));

/// Applies an opened version and records how its content is sealed. Returns true if it changed
/// anything.
fn apply_opened(
    state: &mut FolderState,
    verified: Verified,
    content: Option<SealedContent>,
    now: i64,
) -> bool {
    if let Some((content, (key, chunks))) = content {
        if !state.sealed_chunks.contains_key(&(content, key)) {
            state.sealed_chunks.insert((content, key), chunks);
        }
    }
    state
        .meta
        .apply_verified(verified, now)
        .is_ok_and(|a| a != tidefell_meta::Applied::Stale)
}

/// Keeps a sealed version this device cannot open yet, unless a version it holds supersedes it
/// or it is kept already. Returns true if it was kept.
fn keep_waiting(
    state: &mut FolderState,
    sealed: &SealedVersion,
    signed: Signed<SealedVersion>,
) -> bool {
    let current = state.meta.current_vector(&sealed.entry);
    if current.compare(&sealed.version) == Causality::After {
        return false;
    }
    let item = Item::Version(signed);
    if has_item(state, &item) {
        return false;
    }
    add_item(state, item);
    true
}

/// Opens a grant made to this device's current encryption key and keeps its key. Returns true
/// if it was a key not held before.
fn open_own_grant(inner: &Inner, state: &mut FolderState, grant: &KeyGrant) -> bool {
    if grant.recipient != inner.key.id() || state.keys.contains_key(&grant.key) {
        return false;
    }
    let to = GrantFor {
        folder: grant.folder,
        recipient: grant.recipient,
        recipient_key: grant.recipient_key,
    };
    match open_grant(
        &inner.encryption,
        &to,
        grant.generation,
        &grant.key,
        &grant.enc,
        &grant.sealed,
    ) {
        Ok(key) => {
            hold(state, &key);
            true
        }
        Err(e) => {
            debug!(folder = %state.config.id, error = %e, "a grant to this device does not open");
            false
        }
    }
}

/// Opens the sealed versions that were waiting for a key now held. Returns true if any was
/// applied.
fn open_waiting(
    state: &mut FolderState,
    now: i64,
    touched: &mut Vec<tidefell_proto::EntryId>,
) -> bool {
    let waiting: Vec<(u64, Signed<SealedVersion>)> = state
        .items
        .iter()
        .filter_map(|(s, i)| match i {
            Item::Version(v) => Some((*s, v.clone())),
            _ => None,
        })
        .collect();
    let mut changed = false;
    for (sequence, signed) in waiting {
        let Ok(sealed) = signed.verify_authored() else {
            remove_item(state, &sequence);
            continue;
        };
        match open(state, &sealed, signed) {
            Some(Ok((verified, content))) => {
                touched.push(sealed.entry);
                remove_item(state, &sequence);
                changed |= apply_opened(state, verified, content, now);
            }
            Some(Err(_)) => {
                remove_item(state, &sequence);
            }
            None => {}
        }
    }
    changed
}

/// The signers of `items` whose certificates this device lacks, each once.
pub(crate) fn unknown_authors(inner: &Inner, items: &[Item]) -> Vec<DeviceId> {
    let me = inner.key.id();
    let vouching = inner.vouching.lock().unwrap();
    let mut signers: Vec<DeviceId> = items
        .iter()
        .map(signer_of)
        .filter(|s| *s != me && !vouching.knows(*s))
        .collect();
    signers.sort_unstable();
    signers.dedup();
    signers
}

/// The device's secret for opening grants, derived from its identity key.
pub(crate) fn secret(key: &tidefell_identity::DeviceKey) -> EncryptionSecret {
    EncryptionSecret::for_device(&key.seed())
}

/// Chunk `id` of the folder, sealed as its key seals it, to serve: read from the block store or
/// a file and sealed again, which gives the same bytes every time (`specs/encryption-v1.md`).
pub(crate) fn serve_chunk(inner: &Inner, handle: &FolderHandle, id: &ChunkId) -> Option<Vec<u8>> {
    let (hash, key_id, folder) = {
        let mut state = handle.lock();
        let (hash, key_id) = locate(inner, &mut state, id)?;
        (hash, key_id, state.config.id)
    };
    let data = crate::content::read_chunk(inner, handle, &hash)?;
    let key = key(&handle.lock(), &key_id)?;
    let (sealed_id, sealed) = key.seal_chunk(&folder, &data).ok()?;
    (sealed_id == *id).then_some(sealed)
}

/// Which of the chunks `ids` this device holds, in order.
pub(crate) fn holds(inner: &Inner, state: &mut FolderState, ids: &[ChunkId]) -> Vec<bool> {
    ids.iter()
        .map(|id| {
            locate(inner, state, id)
                .is_some_and(|(hash, _)| crate::content::has_chunk(inner, state, &hash))
        })
        .collect()
}

/// Which chunk `id` is, and its key: as recorded, or found from the content that lists it, if
/// this device holds that content, as after its records were rebuilt (ADR 0034). The chunk is
/// read and checked against the ID before it is recorded.
fn locate(inner: &Inner, state: &mut FolderState, id: &ChunkId) -> Option<(ChunkHash, KeyId)> {
    if let Some(found) = state.chunk_ids.get(id) {
        return Some(*found);
    }
    let (content, key_id, index) =
        state
            .sealed_chunks
            .iter()
            .find_map(|((content, key), list)| {
                list.iter()
                    .position(|(listed, _)| listed == id)
                    .map(|index| (*content, *key, index))
            })?;
    let hash = state.manifests.get(&content)?.chunks.get(index)?.hash;
    let data = crate::content::read_chunk_here(inner, state, &hash, None)?;
    if key(state, &key_id)?.chunk_id(&data) != *id {
        return None;
    }
    record_chunk_id(state, *id, hash, key_id);
    Some((hash, key_id))
}

/// Fetches the chunks of `content` this device lacks from the peers it syncs the folder with,
/// storage peers included, opens and checks each, and keeps it by its plaintext hash. Returns the
/// content's manifest once every chunk is here, which is then written like any other.
pub(crate) async fn fetch_content(
    inner: &std::sync::Arc<Inner>,
    node: &std::sync::Arc<tidefell_net::Node>,
    handle: &FolderHandle,
    folder: tidefell_proto::FolderId,
    content: tidefell_proto::ContentHash,
) -> std::result::Result<tidefell_proto::Manifest, String> {
    // The content as each key held seals it: tried in turn, as peers hold the chunks of the
    // versions they have.
    let candidates: Vec<(FolderKey, Vec<(ChunkId, u32)>)> = {
        let state = handle.lock();
        state
            .sealed_chunks
            .iter()
            .filter(|((c, _), _)| *c == content)
            .filter_map(|((_, key_id), list)| Some((key(&state, key_id)?, list.clone())))
            .collect()
    };
    if candidates.is_empty() {
        return Err("no version here says how this content is sealed, with a key held".into());
    }
    let mut last = String::new();
    for (key, list) in candidates {
        match fetch_sealed(inner, node, handle, folder, content, &key, &list).await {
            Ok(manifest) => return Ok(manifest),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// [`fetch_content`] for the content as `key` seals it, in chunks `list`.
async fn fetch_sealed(
    inner: &std::sync::Arc<Inner>,
    node: &std::sync::Arc<tidefell_net::Node>,
    handle: &FolderHandle,
    folder: tidefell_proto::FolderId,
    content: tidefell_proto::ContentHash,
    key: &FolderKey,
    list: &[(ChunkId, u32)],
) -> std::result::Result<tidefell_proto::Manifest, String> {
    use tidefell_net::{BlobKind, BlobResult};
    use tidefell_proto::blobs2::BlobId;

    let mut hashes: Vec<Option<ChunkHash>> = {
        let state = handle.lock();
        list.iter()
            .map(|(id, _)| {
                state
                    .chunk_ids
                    .get(id)
                    .map(|(hash, _)| *hash)
                    .filter(|hash| crate::content::has_chunk(inner, &state, hash))
            })
            .collect()
    };
    let mut answered = false;
    let peers: Vec<DeviceId> = node
        .encrypted_peers()
        .into_iter()
        .filter(|p| p.shared_folders().contains(&folder))
        .map(|p| p.device())
        .collect();
    for peer in peers {
        let missing: Vec<usize> = (0..list.len()).filter(|i| hashes[*i].is_none()).collect();
        if missing.is_empty() {
            break;
        }
        let Ok(endpoint) = tidefell_net::EndpointId::from_bytes(peer.as_bytes()) else {
            continue;
        };
        for batch in missing.chunks(tidefell_proto::blobs2::MAX_BLOBS_PER_REQUEST) {
            let wanted: Vec<(BlobId, BlobKind)> = batch
                .iter()
                .map(|i| (BlobId::from(list[*i].0), BlobKind::SealedChunk))
                .collect();
            let Ok(results) = node.fetch_blobs(endpoint, folder, &wanted).await else {
                break;
            };
            answered = true;
            for result in results {
                let BlobResult::Found(id, sealed) = result else {
                    continue;
                };
                let chunk_id = ChunkId(id.0);
                // A chunk that does not open, or is not what it was asked for, is as missing.
                let Ok(plaintext) = key.open_chunk(&folder, &chunk_id, &sealed) else {
                    continue;
                };
                let hash = ChunkHash(*blake3::hash(&plaintext).as_bytes());
                if inner
                    .store
                    .lock()
                    .unwrap()
                    .put_verified(hash, &plaintext)
                    .is_err()
                {
                    continue;
                }
                record_chunk_id(&mut handle.lock(), chunk_id, hash, key.id());
                for (i, (listed, _)) in list.iter().enumerate() {
                    if *listed == chunk_id {
                        hashes[i] = Some(hash);
                    }
                }
            }
        }
    }
    if hashes.iter().any(Option::is_none) {
        if answered {
            inner.unobtainable.lock().unwrap().insert((folder, content));
        }
        return Err("some of its chunks are not available from any connected device".into());
    }
    let manifest = tidefell_proto::Manifest {
        content,
        chunks: list
            .iter()
            .zip(hashes)
            .filter_map(|((_, len), hash)| {
                Some(tidefell_proto::ChunkRef {
                    hash: hash?,
                    len: *len,
                })
            })
            .collect(),
    };
    inner
        .unobtainable
        .lock()
        .unwrap()
        .remove(&(folder, content));
    handle.lock().add_manifest(manifest.clone());
    Ok(manifest)
}
