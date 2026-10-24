//! An encrypted folder's versions in `meta` (ADR 0023, "On member devices"): kept with the sealed
//! form they came in, ordered by it only where their decrypted forms tie, and saved and loaded
//! with what was decrypted from them.

use ed25519_dalek::SigningKey;
use shoal_meta::{copy_id, Folder, Retention, Verified, Wire};
use shoal_proto::sync::{EntryKind, EntryVersion};
use shoal_proto::sync2::{ChunkList, SealedVersion};
use shoal_proto::{ContentHash, DeviceId, EntryId, FolderId, KeyId, Signed, VersionVector};

const FOLDER: FolderId = FolderId([1; 32]);
const ENTRY: EntryId = EntryId([2; 16]);

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn id(key: &SigningKey) -> DeviceId {
    DeviceId(key.verifying_key().to_bytes())
}

fn version(author: &SigningKey, vector: VersionVector) -> EntryVersion {
    EntryVersion {
        folder: FOLDER,
        entry: ENTRY,
        path: "notes.txt".into(),
        kind: EntryKind::File {
            content: ContentHash([3; 32]),
            size: 10,
            executable: false,
        },
        modified_ms: 1_000,
        author: id(author),
        version: vector,
    }
}

/// `version` as sealed by its author, with `nonce` standing in for the sealing's randomness.
fn sealed(author: &SigningKey, version: &EntryVersion, nonce: u8) -> Signed<SealedVersion> {
    Signed::sign(
        &SealedVersion {
            folder: version.folder,
            entry: version.entry,
            author: version.author,
            generation: 1,
            key: KeyId([4; 16]),
            chunks: ChunkList::Inline(Vec::new()),
            sealed: vec![nonce; 104],
            version: version.version.clone(),
        },
        author,
    )
}

#[test]
fn a_sealed_version_must_match_what_it_decrypts_to() {
    let author = key(5);
    let v = version(&author, [(id(&author), 1)].into_iter().collect());
    assert!(Verified::sealed(FOLDER, v.clone(), sealed(&author, &v, 1)).is_ok());

    // Opened from one version, claimed for another: refused.
    let mut other = v.clone();
    other.version = [(id(&author), 2)].into_iter().collect();
    assert!(Verified::sealed(FOLDER, other, sealed(&author, &v, 1)).is_err());
    let mut elsewhere = v.clone();
    elsewhere.folder = FolderId([9; 32]);
    assert!(Verified::sealed(FolderId([9; 32]), elsewhere, sealed(&author, &v, 1)).is_err());
    let mut bad_path = v.clone();
    bad_path.path = "../escape".into();
    assert!(Verified::sealed(FOLDER, bad_path, sealed(&author, &v, 1)).is_err());
}

/// Two sealed versions that decrypt alike tie on every rule but the last; every replica picks
/// the same one, whichever arrives first.
#[test]
fn versions_that_decrypt_alike_are_ordered_by_their_copy_ids() {
    let a = key(6);
    let v = version(&a, [(id(&a), 1)].into_iter().collect());
    let (sa, sb) = (sealed(&a, &v, 1), sealed(&a, &v, 2));
    let (ia, ib) = (
        copy_id(&Wire::Sealed(sa.clone())),
        copy_id(&Wire::Sealed(sb.clone())),
    );
    let loser = |first: &Signed<SealedVersion>, second: &Signed<SealedVersion>| {
        let mut folder = Folder::new(FOLDER, id(&key(8)), 1, Retention::Days(30));
        for s in [first, second] {
            let verified = Verified::sealed(FOLDER, v.clone(), s.clone()).unwrap();
            folder.apply_verified(verified, 0).unwrap();
        }
        let copies: Vec<_> = folder.conflicts(&ENTRY).iter().map(|c| c.id).collect();
        assert_eq!(
            copies.len(),
            1,
            "both kept: equal vectors, different encodings"
        );
        copies[0]
    };
    assert_eq!(loser(&sa, &sb), loser(&sb, &sa));
    assert_eq!(loser(&sa, &sb), ia.min(ib), "the greater copy ID wins");
}

#[test]
fn sealed_versions_are_saved_with_what_they_decrypt_to_and_loaded_back() {
    let author = key(9);
    let v = version(&author, [(id(&author), 1)].into_iter().collect());
    let signed = sealed(&author, &v, 3);
    let mut folder = Folder::new(FOLDER, id(&key(10)), 1, Retention::Days(30));
    folder
        .apply_verified(
            Verified::sealed(FOLDER, v.clone(), signed.clone()).unwrap(),
            5,
        )
        .unwrap();
    let journal = folder.take_journal();
    assert_eq!(journal.stored, vec![(1, Wire::Sealed(signed.clone()))]);
    assert_eq!(journal.decrypted, vec![(1, v.clone())]);

    let loaded = Folder::from_records(
        folder.header(),
        [(1, Wire::Sealed(signed.clone()), Some(v.clone()))],
        [],
    )
    .unwrap();
    assert_eq!(loaded.current(&ENTRY).unwrap().to_version(), v);
    // A sealed row without what it decrypted to, or with another version, is refused.
    assert!(Folder::from_records(
        folder.header(),
        [(1, Wire::Sealed(signed.clone()), None)],
        []
    )
    .is_err());
    let mut other = v;
    other.path = "other.txt".into();
    other.version = [(id(&author), 2)].into_iter().collect();
    assert!(Folder::from_records(
        folder.header(),
        [(1, Wire::Sealed(signed), Some(other))],
        []
    )
    .is_err());
}
