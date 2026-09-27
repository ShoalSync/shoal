//! Deterministic conflict resolution between concurrent versions of one entry.

use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::SignedKind;

/// Ordering key for concurrent versions; the greatest key wins. See "Conflict resolution" in
/// `specs/sync-v1.md`.
fn key(v: &EntryVersion) -> (bool, i64, [u8; 32], Vec<u8>) {
    (
        !matches!(v.kind, EntryKind::Tombstone),
        v.modified_ms,
        *v.author.as_bytes(),
        // The encoding the spec defines, which a version without holes ends before the list.
        v.encode(),
    )
}

/// Index of the winning version in `versions`, which must be non-empty.
pub(crate) fn winner<'a>(versions: impl IntoIterator<Item = &'a EntryVersion>) -> usize {
    versions
        .into_iter()
        .map(key)
        .enumerate()
        .max_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(i, _)| i)
        .expect("at least one version")
}
