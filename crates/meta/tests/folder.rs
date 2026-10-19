use ed25519_dalek::SigningKey;
use tidefell_meta::Wire;
use tidefell_meta::{Applied, Error, Folder, Retention};
use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::{ContentHash, DeviceId, EntryId, FolderId, Signed};

const FOLDER: FolderId = FolderId([9; 32]);
const DAY_MS: i64 = 86_400_000;
/// When the replica under test receives remote versions, by its own clock.
const NOW: i64 = 1_000;

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
    assert_eq!(
        a.current(&E).unwrap().to_version().version.get(&device(1)),
        2
    );
    assert_eq!(a.find_path("a.txt").unwrap().to_version().kind, file(2));

    a.local_change(&k, E, "a.txt", EntryKind::Tombstone, 3000)
        .unwrap();
    assert!(a.find_path("a.txt").is_none());
    let recycled = a.recycle_bin().get(&E).unwrap();
    assert_eq!(recycled.version.kind, file(2));
    assert_eq!(recycled.deleted_at_ms, 3000);

    a.restore(&k, E, 4000).unwrap();
    assert_eq!(a.find_path("a.txt").unwrap().to_version().kind, file(2));
    assert!(a.recycle_bin().is_empty());
    assert!(matches!(a.restore(&k, E, 5000), Err(Error::NotRestorable)));
}

#[test]
fn remote_updates_and_stale_versions() {
    let mut a = replica(1);
    let mut b = replica(2);
    let v1 = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    let v2 = a.local_change(&key(1), E, "f", file(2), 2).unwrap();

    assert_eq!(b.apply_remote(v1.clone(), NOW).unwrap(), Applied::New);
    assert_eq!(b.apply_remote(v2.clone(), NOW).unwrap(), Applied::Updated);
    assert_eq!(b.apply_remote(v1, NOW).unwrap(), Applied::Stale);
    assert_eq!(b.apply_remote(v2, NOW).unwrap(), Applied::Stale);
    assert_eq!(b.current(&E).unwrap().to_version().kind, file(2));
}

#[test]
fn concurrent_edits_converge_in_any_order() {
    let mut origin = replica(1);
    let base = origin.local_change(&key(1), E, "f", file(0), 100).unwrap();

    let mut b = replica(2);
    let mut c = replica(3);
    b.apply_remote(base.clone(), NOW).unwrap();
    c.apply_remote(base.clone(), NOW).unwrap();
    let from_b = b.local_change(&key(2), E, "f", file(2), 200).unwrap();
    let from_c = c.local_change(&key(3), E, "f", file(3), 300).unwrap();

    let orders: [&[&Wire]; 3] = [
        &[&base, &from_b, &from_c],
        &[&from_c, &base, &from_b],
        &[&from_b, &from_c, &base],
    ];
    let mut results = Vec::new();
    for order in orders {
        let mut r = replica(4);
        for v in order {
            r.apply_remote((*v).clone(), NOW).unwrap();
        }
        let current = r.current(&E).unwrap().to_version().clone();
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
fn a_change_that_has_seen_every_version_resolves_a_conflict() {
    let mut b = replica(2);
    let mut c = replica(3);
    let vb = b.local_change(&key(2), E, "f", file(2), 1).unwrap();
    let vc = c.local_change(&key(3), E, "f", file(3), 2).unwrap();
    assert_eq!(b.apply_remote(vc, NOW).unwrap(), Applied::Conflict);
    assert_eq!(b.conflicts(&E).len(), 1);

    let all = b.known_vector(&E);
    let (resolved, _) = b
        .local_change_from(&key(2), E, &all, "f", file(4), 3)
        .unwrap();
    assert!(b.conflicts(&E).is_empty());
    assert_eq!(c.apply_remote(vb, NOW).unwrap(), Applied::Conflict);
    assert_eq!(c.apply_remote(resolved, NOW).unwrap(), Applied::Updated);
    assert!(c.conflicts(&E).is_empty());
    assert_eq!(c.current(&E).unwrap().to_version().kind, file(4));
}

#[test]
fn edit_beats_concurrent_delete() {
    let mut a = replica(1);
    let base = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    let mut b = replica(2);
    b.apply_remote(base, NOW).unwrap();

    // a deletes later than b edits, but the edit still wins.
    let delete = a
        .local_change(&key(1), E, "f", EntryKind::Tombstone, 500)
        .unwrap();
    let edit = b.local_change(&key(2), E, "f", file(2), 100).unwrap();
    assert!(a.recycle_bin().get(&E).is_some());

    assert_eq!(a.apply_remote(edit, NOW).unwrap(), Applied::Conflict);
    assert_eq!(b.apply_remote(delete, NOW).unwrap(), Applied::Conflict);
    assert_eq!(a.current(&E).unwrap().to_version().kind, file(2));
    assert_eq!(b.current(&E).unwrap().to_version().kind, file(2));
    assert!(a.recycle_bin().is_empty(), "entry is live again");

    // Nothing to decide (ADR 0029), and the next edit supersedes the delete.
    assert_eq!(a.open_conflicts(&E).count(), 0);
    assert_eq!(b.open_conflicts(&E).count(), 0);
    let next = a.local_change(&key(1), E, "f", file(3), 600).unwrap();
    assert!(a.conflicts(&E).is_empty());
    b.apply_remote(next, NOW).unwrap();
    assert!(b.conflicts(&E).is_empty());
}

/// Deleting a file with an open conflict does not decide it: the delete supersedes only the
/// current version, so the conflict copy, being live, becomes current (ADR 0029).
#[test]
fn deleting_a_conflicted_file_brings_back_the_other_version() {
    let mut a = replica(1);
    let mut b = replica(2);
    let v1 = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    b.apply_remote(v1, NOW).unwrap();
    let va = a.local_change(&key(1), E, "f", file(2), 2).unwrap();
    b.local_change(&key(2), E, "f", file(3), 3).unwrap();
    b.apply_remote(va, NOW).unwrap();
    assert_eq!(b.current(&E).unwrap().to_version().kind, file(3));

    b.local_change(&key(2), E, "f", EntryKind::Tombstone, 4)
        .unwrap();
    assert_eq!(b.current(&E).unwrap().to_version().kind, file(2));
    assert_eq!(b.open_conflicts(&E).count(), 0);
}

/// A delete from a device whose clock is wrong is kept for this device's full retention, counted
/// from when this device received it (ADR 0021).
#[test]
fn remote_deletes_expire_by_this_devices_clock() {
    let received = 400 * DAY_MS;
    for deleting_clock in [0, received + 365 * DAY_MS] {
        let mut a = replica(1);
        let mut b = replica(2);
        let create = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
        b.apply_remote(create, received - DAY_MS).unwrap();
        let delete = a
            .local_change(&key(1), E, "f", EntryKind::Tombstone, deleting_clock)
            .unwrap();
        b.apply_remote(delete, received).unwrap();

        assert_eq!(b.recycle_bin().get(&E).unwrap().deleted_at_ms, received);
        assert!(
            b.purge_expired(received + 30 * DAY_MS - 1).is_empty(),
            "kept for 30 days from receipt, deleting clock {deleting_clock}"
        );
        assert_eq!(b.purge_expired(received + 30 * DAY_MS).len(), 1);
    }
}

#[test]
fn tombstone_for_unknown_entry_is_kept() {
    let mut a = replica(1);
    a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    let delete = a
        .local_change(&key(1), E, "f", EntryKind::Tombstone, 2)
        .unwrap();
    let mut b = replica(2);
    assert_eq!(b.apply_remote(delete, NOW).unwrap(), Applied::New);
    assert_eq!(
        b.current(&E).unwrap().to_version().kind,
        EntryKind::Tombstone
    );
    assert!(b.recycle_bin().is_empty());
}

#[test]
fn index_since_respects_sequence_and_epoch() {
    let mut a = replica(1);
    let e2 = EntryId([2; 16]);
    a.local_change(&key(1), E, "one", file(1), 1).unwrap();
    a.local_change(&key(1), e2, "two", file(2), 2).unwrap();
    a.local_change(&key(1), E, "one", file(3), 3).unwrap();

    let (all, up_to) = a.index_since(0, 0, usize::MAX);
    assert_eq!(up_to, 3);
    assert_eq!(
        all.iter().map(|s| s.sequence).collect::<Vec<_>>(),
        vec![2, 3],
        "superseded versions are not sent"
    );

    let (newer, _) = a.index_since(a.epoch(), 2, usize::MAX);
    assert_eq!(newer.len(), 1);
    assert_eq!(newer[0].sequence, 3);

    let (reset, _) = a.index_since(a.epoch() + 1, 2, usize::MAX);
    assert_eq!(reset.len(), 2, "unknown epoch sends everything");
}

#[test]
fn rejects_invalid_remote_versions() {
    let mut other_folder = Folder::new(FolderId([8; 32]), device(1), 1, Retention::Forever);
    let foreign = other_folder
        .local_change(&key(1), E, "f", file(1), 1)
        .unwrap();
    let mut b = replica(2);
    assert!(matches!(
        b.apply_remote(foreign, NOW),
        Err(Error::WrongFolder)
    ));

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
    assert!(matches!(b.apply_remote(signed, NOW), Err(Error::Proto(_))));

    let traversal = EntryVersion {
        path: "../etc/passwd".into(),
        author: device(3),
        ..forged
    };
    let signed = Signed::sign(&traversal, &key(3));
    assert!(matches!(
        b.apply_remote(signed, NOW),
        Err(Error::InvalidPath(_))
    ));

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
        b.apply_remote(version.clone(), NOW).unwrap();
    }
    assert_eq!(a.recycle_bin().next_expiry_ms(), Some(31 * DAY_MS));

    let discarded = a.discard(&E).unwrap();
    assert_eq!(discarded.version.path, "f");
    assert!(a.discard(&E).is_none());
    assert!(matches!(a.restore(&k, E, 0), Err(Error::NotRestorable)));
    assert_eq!(a.recycle_bin().next_expiry_ms(), Some(33 * DAY_MS));

    // A peer re-sending the delete does not bring it back.
    for version in &sent {
        a.apply_remote(version.clone(), NOW).unwrap();
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
    b.apply_remote(vc, NOW).unwrap();
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
    assert_eq!(
        loaded.index_since(0, 0, usize::MAX),
        b.index_since(0, 0, usize::MAX)
    );

    let mut corrupt = b.to_snapshot();
    corrupt[0] = b'X';
    assert!(matches!(
        Folder::from_snapshot(&corrupt),
        Err(Error::UnsupportedFormat)
    ));
}

/// A version from `author` with an explicit vector, as a restored or cloned device might sign.
fn version_with(author: u8, kind: EntryKind, modified_ms: i64, vector: &[(u8, u64)]) -> Wire {
    let version = EntryVersion {
        folder: FOLDER,
        entry: E,
        path: "f".into(),
        kind,
        modified_ms,
        author: device(author),
        version: vector.iter().map(|&(d, c)| (device(d), c)).collect(),
    };
    Wire::Plain(Signed::sign(&version, &key(author)))
}

#[test]
fn equal_vectors_with_different_content_converge() {
    // ADR 0024: a device restored from a backup signs a second version with the same vector.
    let first = version_with(1, EntryKind::Directory, 5, &[(1, 1)]);
    let second = version_with(1, EntryKind::Symlink { target: "x".into() }, 5, &[(1, 1)]);

    let mut r1 = replica(2);
    assert_eq!(r1.apply_remote(first.clone(), NOW).unwrap(), Applied::New);
    assert_eq!(
        r1.apply_remote(second.clone(), NOW).unwrap(),
        Applied::Conflict
    );
    let mut r2 = replica(3);
    r2.apply_remote(second.clone(), NOW).unwrap();
    r2.apply_remote(first.clone(), NOW).unwrap();

    assert_eq!(
        r1.current(&E),
        r2.current(&E),
        "replicas pick the same winner"
    );
    assert_eq!(r1.conflicts(&E).len(), 1);
    assert_eq!(r2.conflicts(&E).len(), 1);
    // The identical version is still recognised as known.
    assert_eq!(r1.apply_remote(first, NOW).unwrap(), Applied::Stale);
}

#[test]
fn counters_have_a_ceiling() {
    let ceiling = tidefell_proto::MAX_COUNTER;
    let mut b = replica(2);
    // A peer may not send a counter above the ceiling, for any device.
    let over = version_with(1, file(1), 1, &[(1, 1), (2, ceiling + 1)]);
    assert!(matches!(
        b.apply_remote(over, NOW),
        Err(Error::CounterCeiling)
    ));
    assert!(b.current(&E).is_none());

    // At the ceiling a device refuses to make another change instead of wrapping or panicking.
    let at = version_with(1, file(1), 1, &[(1, 1), (2, ceiling)]);
    assert_eq!(b.apply_remote(at, NOW).unwrap(), Applied::New);
    assert!(matches!(
        b.local_change(&key(2), E, "f", file(2), 2),
        Err(Error::CounterCeiling)
    ));
}

#[test]
fn tidefell_directory_is_reserved() {
    let mut a = replica(1);
    for path in [".tidefell", ".tidefell/tmp/x", ".tidefell/mount/y"] {
        assert!(
            matches!(
                a.local_change(&key(1), E, path, file(1), 1),
                Err(Error::InvalidPath(_))
            ),
            "{path}"
        );
    }
    // Only the first component is reserved.
    a.local_change(&key(1), E, "docs/.tidefell", file(1), 1)
        .unwrap();
}

#[test]
fn change_to_older_content_keeps_the_newer_remote_version() {
    // ADR 0024: B edits a file whose disk content is v1 while A's v2 is already known to B but
    // was never written to disk. B's change must not supersede v2.
    let mut a = replica(1);
    let mut b = replica(2);
    let v1 = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    b.apply_remote(v1.clone(), NOW).unwrap();
    let on_disk = b.known_vector(&E);
    let v2 = a.local_change(&key(1), E, "f", file(2), 2).unwrap();
    b.apply_remote(v2.clone(), NOW).unwrap();

    let (edit, _) = b
        .local_change_from(&key(2), E, &on_disk, "f", file(3), 3)
        .unwrap();
    assert_eq!(b.conflicts(&E).len(), 1, "v2 is kept as a conflict copy");
    assert_eq!(b.current(&E).unwrap().to_version().kind, file(3));
    assert_eq!(b.conflicts(&E)[0].version.kind, file(2));

    // A sees the same conflict.
    assert_eq!(a.apply_remote(edit, NOW).unwrap(), Applied::Conflict);
    assert_eq!(a.current(&E), b.current(&E));
}

#[test]
fn a_change_on_a_stale_base_leaves_a_hole() {
    let mut a = replica(1);
    a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    let second = a.local_change(&key(1), E, "f", file(2), 2).unwrap();
    // A stale base that predates this device's second version.
    let stale = [(device(1), 1)].into_iter().collect();
    let (_, third) = a
        .local_change_from(&key(1), E, &stale, "f", file(3), 3)
        .unwrap();
    // Numbered after the second, which it has not seen (ADR 0029).
    assert_eq!(third.get(&device(1)), 3);
    assert!(!third.contains(&device(1), 2));
    assert_eq!(a.conflicts(&E).len(), 1);
    assert_eq!(a.conflicts(&E)[0].id, tidefell_meta::copy_id(&second));
}

/// A and B edit apart and B's edit wins. Edits to the winner, on any device, keep A's version
/// until the conflict is resolved (ADR 0029).
#[test]
fn a_conflict_lasts_through_later_edits() {
    let mut a = replica(1);
    let mut b = replica(2);
    let mut c = replica(3);
    let v1 = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    b.apply_remote(v1.clone(), NOW).unwrap();
    c.apply_remote(v1, NOW).unwrap();
    let va = a.local_change(&key(1), E, "f", file(2), 2).unwrap();
    let vb = b.local_change(&key(2), E, "f", file(3), 3).unwrap();
    a.apply_remote(vb.clone(), NOW).unwrap();
    b.apply_remote(va.clone(), NOW).unwrap();
    c.apply_remote(va.clone(), NOW).unwrap();
    c.apply_remote(vb, NOW).unwrap();
    for replica in [&a, &b, &c] {
        assert_eq!(replica.current(&E).unwrap().to_version().kind, file(3));
        assert_eq!(replica.open_conflicts(&E).count(), 1);
    }

    // A, whose version lost, edits the winner twice; then B and C each edit what they got.
    let mut edits = Vec::new();
    for n in 4..6 {
        let base = a.current_vector(&E);
        let (edit, _) = a
            .local_change_from(&key(1), E, &base, "f", file(n), i64::from(n))
            .unwrap();
        edits.push(edit);
    }
    for edit in &edits {
        b.apply_remote(edit.clone(), NOW).unwrap();
    }
    edits.push(b.local_change(&key(2), E, "f", file(6), 6).unwrap());
    for edit in &edits {
        c.apply_remote(edit.clone(), NOW).unwrap();
    }
    edits.push(c.local_change(&key(3), E, "f", file(7), 7).unwrap());
    for replica in [&mut a, &mut b] {
        for edit in &edits {
            replica.apply_remote(edit.clone(), NOW).unwrap();
        }
    }
    for replica in [&a, &b, &c] {
        let conflicts: Vec<_> = replica.open_conflicts(&E).collect();
        assert_eq!(conflicts.len(), 1, "A's version is still there");
        assert_eq!(conflicts[0].id, tidefell_meta::copy_id(&va));
        assert_eq!(replica.current(&E), c.current(&E));
    }
}

/// Two devices that make the same change apart do not conflict, and the next change supersedes
/// both (ADR 0029).
#[test]
fn the_same_content_is_not_a_conflict() {
    let mut a = replica(1);
    let mut b = replica(2);
    let v1 = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    b.apply_remote(v1, NOW).unwrap();
    let va = a.local_change(&key(1), E, "f", file(2), 2).unwrap();
    let vb = b.local_change(&key(2), E, "f", file(2), 3).unwrap();
    a.apply_remote(vb, NOW).unwrap();
    b.apply_remote(va, NOW).unwrap();
    assert_eq!(a.conflicts(&E).len(), 1);
    assert_eq!(a.open_conflicts(&E).count(), 0);
    let next = a.local_change(&key(1), E, "f", file(3), 4).unwrap();
    assert!(a.conflicts(&E).is_empty());
    b.apply_remote(next, NOW).unwrap();
    assert!(b.conflicts(&E).is_empty());
}

#[test]
fn index_pages_resume_where_they_stopped() {
    let mut a = replica(1);
    for n in 0..10u8 {
        a.local_change(&key(1), EntryId([n; 16]), &format!("f{n}"), file(n), 1)
            .unwrap();
    }
    let mut since = 0;
    let mut seen = Vec::new();
    loop {
        let (page, up_to) = a.index_since(a.epoch(), since, 3);
        assert!(page.len() <= 3);
        assert!(page.windows(2).all(|w| w[0].sequence < w[1].sequence));
        seen.extend(page.iter().map(|s| s.sequence));
        let done = page.len() < 3;
        since = up_to;
        if done {
            break;
        }
    }
    assert_eq!(seen, (1..=10).collect::<Vec<_>>());
    assert_eq!(since, 10, "the last page reports the index's sequence");
}

/// A and B edit apart; B's version wins. Returns the three replicas, each holding both, with
/// A's version as the open conflict.
fn conflicted() -> (Folder, Folder, Folder, Wire, Wire) {
    let mut a = replica(1);
    let mut b = replica(2);
    let mut c = replica(3);
    let v1 = a.local_change(&key(1), E, "notes.txt", file(1), 1).unwrap();
    b.apply_remote(v1.clone(), NOW).unwrap();
    c.apply_remote(v1, NOW).unwrap();
    let va = a.local_change(&key(1), E, "notes.txt", file(2), 2).unwrap();
    let vb = b.local_change(&key(2), E, "notes.txt", file(3), 3).unwrap();
    a.apply_remote(vb.clone(), NOW).unwrap();
    b.apply_remote(va.clone(), NOW).unwrap();
    c.apply_remote(va.clone(), NOW).unwrap();
    c.apply_remote(vb.clone(), NOW).unwrap();
    (a, b, c, va, vb)
}

#[test]
fn resolving_keeps_the_other_version_in_the_recycle_bin() {
    let (mut a, mut b, _, va, _) = conflicted();
    // B keeps its own version.
    let all = b.known_vector(&E);
    let (resolve, _) = b
        .local_change_from(&key(2), E, &all, "notes.txt", file(3), 4)
        .unwrap();
    a.apply_remote(resolve, NOW + 5).unwrap();
    let id = tidefell_meta::copy_id(&va);
    for r in [&a, &b] {
        assert!(r.conflicts(&E).is_empty());
        let kept = r.recycle_bin().get(&id).expect("A's version is in the bin");
        assert_eq!(kept.conflict_of, Some(E));
        assert_eq!(kept.version.kind, file(2));
        assert_eq!(kept.version.path, "notes.txt");
    }

    // Restoring it makes it a file of its own, the same on every device that does.
    let restored = a
        .restore_copy(&key(1), id, "notes (Laptop, 2026-09-27).txt", NOW + 6)
        .unwrap();
    assert!(a.recycle_bin().get(&id).is_none());
    assert_eq!(a.current(&id).unwrap().to_version().kind, file(2));
    b.apply_remote(restored, NOW + 7).unwrap();
    assert!(b.recycle_bin().get(&id).is_none());
    assert!(matches!(
        b.restore_copy(&key(2), id, "x", NOW),
        Err(Error::NotRestorable)
    ));
}

#[test]
fn editing_the_losing_version_does_not_resolve() {
    // A still has its own version on disk and edits it: that supersedes only A's version.
    let (mut a, mut b, _, va, _) = conflicted();
    let own = va.plain().unwrap().verify_authored().unwrap().version;
    let (edit, _) = a
        .local_change_from(&key(1), E, &own, "notes.txt", file(4), 5)
        .unwrap();
    b.apply_remote(edit, NOW).unwrap();
    for r in [&a, &b] {
        assert!(r.recycle_bin().is_empty());
        assert_eq!(r.open_conflicts(&E).count(), 1);
    }
}

#[test]
fn an_edit_made_before_the_resolve_survives_it() {
    let (_, mut b, mut c, va, _) = conflicted();
    let edit = c.local_change(&key(3), E, "notes.txt", file(5), 5).unwrap();
    let all = b.known_vector(&E);
    let (resolve, _) = b
        .local_change_from(&key(2), E, &all, "notes.txt", file(3), 4)
        .unwrap();
    b.apply_remote(edit, NOW).unwrap();
    c.apply_remote(resolve, NOW).unwrap();
    for r in [&b, &c] {
        assert!(r.recycle_bin().get(&tidefell_meta::copy_id(&va)).is_some());
        assert_eq!(r.current(&E).unwrap().to_version().kind, file(5));
        assert_eq!(r.open_conflicts(&E).count(), 1, "the edit and the resolve");
    }
}

#[test]
fn keeping_both_leaves_nothing_in_the_bin_whatever_arrives_first() {
    let (_, mut b, _, va, _) = conflicted();
    let id = tidefell_meta::copy_id(&va);
    let (copy, _) = b
        .local_change_from(
            &key(2),
            id,
            &Default::default(),
            "notes (Laptop, 2026-09-27).txt",
            file(2),
            2,
        )
        .unwrap();
    let all = b.known_vector(&E);
    let (resolve, _) = b
        .local_change_from(&key(2), E, &all, "notes.txt", file(3), 4)
        .unwrap();
    assert!(b.recycle_bin().is_empty());
    for order in [[&copy, &resolve], [&resolve, &copy]] {
        let (mut a, ..) = conflicted();
        for v in order {
            a.apply_remote(v.clone(), NOW).unwrap();
        }
        assert!(a.recycle_bin().is_empty());
        assert_eq!(a.current(&id).unwrap().to_version().kind, file(2));
        assert!(a.conflicts(&E).is_empty());
    }
}

#[test]
fn keeping_the_other_version_bins_the_winner() {
    let (mut a, mut b, _, _, vb) = conflicted();
    let all = a.known_vector(&E);
    let (resolve, _) = a
        .local_change_from(&key(1), E, &all, "notes.txt", file(2), 4)
        .unwrap();
    b.apply_remote(resolve, NOW).unwrap();
    let id = tidefell_meta::copy_id(&vb);
    for r in [&a, &b] {
        assert_eq!(r.current(&E).unwrap().to_version().kind, file(2));
        assert_eq!(r.recycle_bin().get(&id).unwrap().version.kind, file(3));
    }
}

#[test]
fn ordinary_edits_bin_nothing() {
    // An edit beats a delete, then the next edit supersedes both.
    let mut a = replica(1);
    let mut b = replica(2);
    let v1 = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    b.apply_remote(v1, NOW).unwrap();
    let delete = a
        .local_change(&key(1), E, "f", EntryKind::Tombstone, 2)
        .unwrap();
    let edit = b.local_change(&key(2), E, "f", file(2), 3).unwrap();
    a.apply_remote(edit, NOW).unwrap();
    b.apply_remote(delete, NOW).unwrap();
    let next = b.local_change(&key(2), E, "f", file(3), 4).unwrap();
    a.apply_remote(next, NOW).unwrap();
    for r in [&a, &b] {
        assert!(r.conflicts(&E).is_empty());
        assert!(r.recycle_bin().is_empty());
    }
}

#[test]
fn changes_are_noted_for_the_activity_log() {
    use tidefell_meta::ChangeKind;

    let mut a = replica(1);
    let mut b = replica(2);
    let v1 = a.local_change(&key(1), E, "f", file(1), 1).unwrap();
    let v2 = a.local_change(&key(1), E, "f", file(2), 2).unwrap();
    let v3 = a.local_change(&key(1), E, "g", file(2), 3).unwrap();
    let v4 = a
        .local_change(&key(1), E, "g", EntryKind::Tombstone, 4)
        .unwrap();
    let kinds: Vec<(ChangeKind, String)> = a
        .take_changes()
        .into_iter()
        .map(|c| (c.kind, c.path))
        .collect();
    assert_eq!(
        kinds,
        vec![
            (ChangeKind::Added, "f".into()),
            (ChangeKind::Changed, "f".into()),
            (ChangeKind::Renamed, "g".into()),
            (ChangeKind::Deleted, "g".into()),
        ]
    );
    assert!(a.take_changes().is_empty(), "taken once");

    // A peer's copy notes who made each change, and nothing for versions already known.
    for v in [&v1, &v2, &v3, &v4] {
        b.apply_remote(v.clone(), NOW).unwrap();
    }
    b.apply_remote(v4, NOW).unwrap();
    let changes = b.take_changes();
    assert_eq!(changes.len(), 4);
    assert!(changes.iter().all(|c| c.author == device(1)));
    assert_eq!(changes[2].old_path.as_deref(), Some("f"));

    // A concurrent edit that loses is a conflict.
    let mut c = replica(3);
    let base = a.local_change(&key(1), E, "g", file(5), 5).unwrap();
    b.apply_remote(base.clone(), NOW).unwrap();
    c.apply_remote(base, NOW).unwrap();
    b.take_changes();
    let winner = b.local_change(&key(2), E, "g", file(6), 7).unwrap();
    let loser = c.local_change(&key(3), E, "g", file(7), 6).unwrap();
    b.take_changes();
    c.take_changes();
    b.apply_remote(loser, NOW).unwrap();
    let changes = b.take_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, ChangeKind::Conflict);
    assert_eq!(changes[0].author, device(3));
    // On the other side the arriving version wins, and the local one waits: a conflict too.
    c.apply_remote(winner, NOW).unwrap();
    let changes = c.take_changes();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, ChangeKind::Conflict);
    assert_eq!(changes[0].author, device(2));
}

#[test]
fn replicas_taking_over_one_version_agree_without_a_conflict() {
    // ADR 0040's takeover: two replicas each make a change of their own with the content of a
    // version by a device since revoked. The two are concurrent, with the same content: settled,
    // so neither is an open conflict, wherever they meet.
    let mut lost = replica(1);
    let made = lost
        .local_change(&key(1), EntryId([5; 16]), "photo.jpg", file(7), 100)
        .unwrap();
    let (mut a, mut b) = (replica(2), replica(3));
    a.apply_remote(made.clone(), 1).unwrap();
    b.apply_remote(made, 1).unwrap();
    let take_over = |folder: &mut Folder, n: u8| {
        let base = folder.current_vector(&EntryId([5; 16]));
        folder
            .local_change_from(&key(n), EntryId([5; 16]), &base, "photo.jpg", file(7), 100)
            .unwrap()
            .0
    };
    let by_a = take_over(&mut a, 2);
    let by_b = take_over(&mut b, 3);
    a.apply_remote(by_b, 2).unwrap();
    b.apply_remote(by_a, 2).unwrap();
    for folder in [&a, &b] {
        let current = folder.current(&EntryId([5; 16])).unwrap();
        assert_eq!(*current.kind, file(7));
        assert_ne!(
            current.author(),
            device(1),
            "no longer the revoked device's"
        );
        assert_eq!(folder.open_conflicts(&EntryId([5; 16])).count(), 0);
    }
    assert_eq!(
        a.current(&EntryId([5; 16])).unwrap().author(),
        b.current(&EntryId([5; 16])).unwrap().author(),
        "the same winner on both"
    );
}

/// Where entries go stays right as their versions change, with the layout looked at between
/// each change: one that leaves every live entry's path and kind alike keeps the layout, and
/// any other lays the folder out again.
#[test]
fn the_layout_follows_each_change() {
    let mut a = replica(1);
    let k = key(1);
    let (low, high) = (EntryId([1; 16]), EntryId([2; 16]));
    let at = |a: &Folder, path: &str| a.find_path(path).map(|v| v.entry);
    a.local_change(&k, low, "a.txt", file(1), 1000).unwrap();
    a.local_change(&k, high, "b.txt", file(2), 1000).unwrap();
    assert_eq!(at(&a, "b.txt"), Some(high));

    // New content alone.
    a.local_change(&k, high, "b.txt", file(3), 2000).unwrap();
    assert_eq!(at(&a, "b.txt"), Some(high));
    assert_eq!(a.disk_path(&high), Some("b.txt"));

    // A move onto another entry's path: the lower entry ID keeps it (ADR 0024).
    a.local_change(&k, high, "a.txt", file(3), 3000).unwrap();
    assert_eq!(at(&a, "b.txt"), None);
    assert_eq!(at(&a, "a.txt"), Some(low));
    let beside = a.disk_path(&high).unwrap().to_owned();
    assert_ne!(beside, "a.txt");
    assert_eq!(at(&a, &beside), Some(high));

    // New content for the one that keeps the path: the other still gives way.
    a.local_change(&k, low, "a.txt", file(4), 4000).unwrap();
    assert_eq!(a.disk_path(&high), Some(beside.as_str()));

    // A delete: the other takes the path.
    a.local_change(&k, low, "a.txt", EntryKind::Tombstone, 5000)
        .unwrap();
    assert_eq!(a.disk_path(&low), None);
    assert_eq!(at(&a, "a.txt"), Some(high));

    // A file that becomes a folder, then the deleted file back: a folder keeps its place.
    a.local_change(&k, high, "a.txt", EntryKind::Directory, 6000)
        .unwrap();
    assert_eq!(at(&a, "a.txt"), Some(high));
    a.restore(&k, low, 7000).unwrap();
    assert_eq!(at(&a, "a.txt"), Some(high));
    assert_ne!(a.disk_path(&low), Some("a.txt"));
}
