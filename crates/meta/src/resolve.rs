//! Deterministic conflict resolution between concurrent versions of one entry.

use shoal_proto::sync::{EntryKind, EntryVersion};
use shoal_proto::{EntryId, SignedKind};

/// Ordering key for concurrent versions; the greatest key wins. See "Conflict resolution" in
/// `specs/sync-v1.md`, and in `specs/sync-v2.md` for the last part: two encrypted versions that
/// decrypt alike are told apart by their conflict copy IDs. Two plaintext versions that encode
/// alike are the same signed version, so it changes nothing for them.
fn key((v, id): (&EntryVersion, &EntryId)) -> (bool, i64, [u8; 32], Vec<u8>, [u8; 16]) {
    (
        !matches!(v.kind, EntryKind::Tombstone),
        v.modified_ms,
        *v.author.as_bytes(),
        // The encoding the spec defines, which a version without holes ends before the list.
        v.encode(),
        *id.as_bytes(),
    )
}

/// Index of the winning version in `versions`, each with its conflict copy ID, which must be
/// non-empty.
pub(crate) fn winner<'a>(
    versions: impl IntoIterator<Item = (&'a EntryVersion, &'a EntryId)>,
) -> usize {
    versions
        .into_iter()
        .map(key)
        .enumerate()
        .max_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(i, _)| i)
        .expect("at least one version")
}
