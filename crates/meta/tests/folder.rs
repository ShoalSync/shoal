use ed25519_dalek::SigningKey;
use tidefell_meta::{Applied, Error, Folder, Retention};
use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::{ContentHash, DeviceId, EntryId, FolderId, Signed};

const FOLDER: FolderId = FolderId([9; 32]);
const DAY_MS: i64 = 86_400_000;

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn device(n: u8) -> DeviceId {
    DeviceId(key(n).verifying_key().to_bytes())
}

fn replica(n: u8) -> Folder {
    Folder::new(FOLDER, device(n), u64::from(n) + 100, Retention::Days(30))
}

fn file(content: u8) -> EntryKind {
    EntryKind::File {
        content: ContentHash([content; 32]),
        size: 10,
        executable: false,
    }
}

const E: EntryId = EntryId([1; 16]);

#[test]
fn create_update_delete_restore() {
    let mut a = replica(1);
    let k = key(1);
    a.local_change(&k, E, "a.txt", file(1), 1000).unwrap();
    a.local_change(&k, E, "a.txt", file(2), 2000).unwrap();
    assert_eq!(a.current(&E).unwrap().version.get(&device(1)), 2);
    assert_eq!(a.find_path("a.txt").unwrap().kind, file(2));

    a.local_change(&k, E, "a.txt", EntryKind::Tombstone, 3000)
        .unwrap();
    assert!(a.find_path("a.txt").is_none());
    let recycled = a.recycle_bin().get(&E).unwrap();
    assert_eq!(recycled.version.kind, file(2));
    assert_eq!(recycled.deleted_at_ms, 3000);

    a.restore(&k, E, 4000).unwrap();
    assert_eq!(a.find_path("a.txt").unwrap().kind, file(2));
    assert!(a.recycle_bin().is_empty());
    assert!(matches!(a.restore(&k, E, 5000), Err(Error::NotRestorable)));
}

#[test]
fn remote_updates_and_stale_versions() {
    let mut a = replica(1);
    let mut b = replica(2);
    let v1 = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    let v2 = a.local_change(&key(1), E, "f", file(2), 2).unwrap();

    assert_eq!(b.apply_remote(v1.clone()).unwrap(), Applied::New);
    assert_eq!(b.apply_remote(v2.clone()).unwrap(), Applied::Updated);
    assert_eq!(b.apply_remote(v1).unwrap(), Applied::Stale);
    assert_eq!(b.apply_remote(v2).unwrap(), Applied::Stale);
    assert_eq!(b.current(&E).unwrap().kind, file(2));
}

#[test]
fn concurrent_edits_converge_in_any_order() {
    let mut origin = replica(1);
    let base = origin.local_change(&key(1), E, "f", file(0), 100).unwrap();

    let mut b = replica(2);
    let mut c = replica(3);
    b.apply_remote(base.clone()).unwrap();
    c.apply_remote(base.clone()).unwrap();
    let from_b = b.local_change(&key(2), E, "f", file(2), 200).unwrap();
    let from_c = c.local_change(&key(3), E, "f", file(3), 300).unwrap();

    let orders: [&[&Signed<EntryVersion>]; 3] = [
        &[&base, &from_b, &from_c],
        &[&from_c, &base, &from_b],
        &[&from_b, &from_c, &base],
    ];
    let mut results = Vec::new();
    for order in orders {
        let mut r = replica(4);
        for v in order {
            r.apply_remote((*v).clone()).unwrap();
        }
        let current = r.current(&E).unwrap().clone();
        let conflicts: Vec<_> = r.conflicts(&E).iter().map(|s| s.version.clone()).collect();
        results.push((current, conflicts));
    }
    assert!(results.windows(2).all(|w| w[0] == w[1]));
    let (current, conflicts) = &results[0];
    assert_eq!(current.kind, file(3), "later modification wins");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].kind, file(2));
}

#[test]
fn local_change_resolves_conflict() {
    let mut b = replica(2);
    let mut c = replica(3);
    let vb = b.local_change(&key(2), E, "f", file(2), 1).unwrap();
    let vc = c.local_change(&key(3), E, "f", file(3), 2).unwrap();
    assert_eq!(b.apply_remote(vc).unwrap(), Applied::Conflict);
    assert_eq!(b.conflicts(&E).len(), 1);

    let resolved = b.local_change(&key(2), E, "f", file(4), 3).unwrap();
    assert!(b.conflicts(&E).is_empty());
    assert_eq!(c.apply_remote(vb).unwrap(), Applied::Conflict);
    assert_eq!(c.apply_remote(resolved).unwrap(), Applied::Updated);
    assert!(c.conflicts(&E).is_empty());
    assert_eq!(c.current(&E).unwrap().kind, file(4));
}

#[test]
fn edit_beats_concurrent_delete() {
    let mut a = replica(1);
    let base = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    let mut b = replica(2);
    b.apply_remote(base).unwrap();

    // a deletes later than b edits, but the edit still wins.
    let delete = a
        .local_change(&key(1), E, "f", EntryKind::Tombstone, 500)
        .unwrap();
    let edit = b.local_change(&key(2), E, "f", file(2), 100).unwrap();
    assert!(a.recycle_bin().get(&E).is_some());

    assert_eq!(a.apply_remote(edit).unwrap(), Applied::Conflict);
    assert_eq!(b.apply_remote(delete).unwrap(), Applied::Conflict);
    assert_eq!(a.current(&E).unwrap().kind, file(2));
    assert_eq!(b.current(&E).unwrap().kind, file(2));
    assert!(a.recycle_bin().is_empty(), "entry is live again");
}

#[test]
fn tombstone_for_unknown_entry_is_kept() {
    let mut a = replica(1);
    a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    let delete = a
        .local_change(&key(1), E, "f", EntryKind::Tombstone, 2)
        .unwrap();
    let mut b = replica(2);
    assert_eq!(b.apply_remote(delete).unwrap(), Applied::New);
    assert_eq!(b.current(&E).unwrap().kind, EntryKind::Tombstone);
    assert!(b.recycle_bin().is_empty());
}

#[test]
fn index_since_respects_sequence_and_epoch() {
    let mut a = replica(1);
    let e2 = EntryId([2; 16]);
    a.local_change(&key(1), E, "one", file(1), 1).unwrap();
    a.local_change(&key(1), e2, "two", file(2), 2).unwrap();
    a.local_change(&key(1), E, "one", file(3), 3).unwrap();

    let (all, up_to) = a.index_since(0, 0);
    assert_eq!(up_to, 3);
    assert_eq!(
        all.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![2, 3],
        "superseded versions are not sent"
    );

    let (newer, _) = a.index_since(a.epoch(), 2);
    assert_eq!(newer.len(), 1);
    assert_eq!(newer[0].version.kind, file(3));

    let (reset, _) = a.index_since(a.epoch() + 1, 2);
    assert_eq!(reset.len(), 2, "unknown epoch sends everything");
}

#[test]
fn rejects_invalid_remote_versions() {
    let mut other_folder = Folder::new(FolderId([8; 32]), device(1), 1, Retention::Forever);
    let foreign = other_folder
        .local_change(&key(1), E, "f", file(1), 1)
        .unwrap();
    let mut b = replica(2);
    assert!(matches!(b.apply_remote(foreign), Err(Error::WrongFolder)));

    // A version whose author is not the signer.
    let forged = EntryVersion {
        folder: FOLDER,
        entry: E,
        path: "f".into(),
        kind: file(1),
        modified_ms: 1,
        author: device(1),
        version: [(device(1), 1)].into_iter().collect(),
    };
    let signed = Signed::sign(&forged, &key(3));
    assert!(matches!(b.apply_remote(signed), Err(Error::Proto(_))));

    let traversal = EntryVersion {
        path: "../etc/passwd".into(),
        author: device(3),
        ..forged
    };
    let signed = Signed::sign(&traversal, &key(3));
    assert!(matches!(b.apply_remote(signed), Err(Error::InvalidPath(_))));

    let mut a = replica(1);
    assert!(matches!(
        a.local_change(&key(2), E, "f", file(1), 1),
        Err(Error::WrongDevice)
    ));
}

#[test]
fn recycle_bin_retention() {
    let mut a = replica(1);
    let k = key(1);
    a.local_change(&k, E, "f", file(1), 0).unwrap();
    a.local_change(&k, E, "f", EntryKind::Tombstone, DAY_MS)
        .unwrap();

    assert!(a.purge_expired(30 * DAY_MS).is_empty());
    let purged = a.purge_expired(31 * DAY_MS);
    assert_eq!(purged.len(), 1);
    assert_eq!(purged[0].version.kind, file(1));
    assert!(a.recycle_bin().is_empty());

    let e2 = EntryId([2; 16]);
    a.set_retention(Retention::Forever);
    a.local_change(&k, e2, "g", file(2), 0).unwrap();
    a.local_change(&k, e2, "g", EntryKind::Tombstone, 0)
        .unwrap();
    assert!(a.purge_expired(i64::MAX).is_empty());
}

#[test]
fn discarding_empties_the_local_recycle_bin() {
    let mut a = replica(1);
    let mut b = replica(2);
    let k = key(1);
    let e2 = EntryId([2; 16]);
    let mut sent = Vec::new();
    for (entry, path, deleted) in [(E, "f", DAY_MS), (e2, "g", 3 * DAY_MS)] {
        sent.push(a.local_change(&k, entry, path, file(1), 0).unwrap());
        sent.push(
            a.local_change(&k, entry, path, EntryKind::Tombstone, deleted)
                .unwrap(),
        );
    }
    for version in &sent {
        b.apply_remote(version.clone()).unwrap();
    }
    assert_eq!(a.recycle_bin().next_expiry_ms(), Some(31 * DAY_MS));

    let discarded = a.discard(&E).unwrap();
    assert_eq!(discarded.version.path, "f");
    assert!(a.discard(&E).is_none());
    assert!(matches!(a.restore(&k, E, 0), Err(Error::NotRestorable)));
    assert_eq!(a.recycle_bin().next_expiry_ms(), Some(33 * DAY_MS));

    // A peer re-sending the delete does not bring it back.
    for version in &sent {
        a.apply_remote(version.clone()).unwrap();
    }
    assert!(a.recycle_bin().get(&E).is_none());

    assert_eq!(a.discard_all().len(), 1);
    assert!(a.recycle_bin().is_empty());
    assert_eq!(a.recycle_bin().next_expiry_ms(), None);
    // Other replicas keep their copies.
    assert_eq!(b.recycle_bin().len(), 2);
}

#[test]
fn snapshot_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("folder.meta");

    let mut b = replica(2);
    let mut c = replica(3);
    b.local_change(&key(2), E, "f", file(2), 1).unwrap();
    let vc = c.local_change(&key(3), E, "f", file(3), 2).unwrap();
    b.apply_remote(vc).unwrap();
    let e2 = EntryId([2; 16]);
    b.local_change(&key(2), e2, "g", file(5), 1).unwrap();
    b.local_change(&key(2), e2, "g", EntryKind::Tombstone, 9)
        .unwrap();
    b.save(&path).unwrap();

    let loaded = Folder::load(&path).unwrap();
    assert_eq!(loaded.sequence(), b.sequence());
    assert_eq!(loaded.epoch(), b.epoch());
    assert_eq!(loaded.current(&E), b.current(&E));
    assert_eq!(loaded.conflicts(&E), b.conflicts(&E));
    assert_eq!(loaded.recycle_bin(), b.recycle_bin());
    assert_eq!(loaded.index_since(0, 0), b.index_since(0, 0));

    let mut corrupt = b.to_snapshot();
    corrupt[0] = b'X';
    assert!(matches!(
        Folder::from_snapshot(&corrupt),
        Err(Error::UnsupportedFormat)
    ));
}
