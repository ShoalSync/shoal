//! Convergence (ADR 0031): replicas that apply the same versions in any order reach the same
//! winners and conflict copies. The recycle bin keeps what each replica held (ADR 0029: a delete
//! or a decision recycles on the replicas that stored the version it replaced), so it can differ
//! with the order; what it holds is checked instead.

use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;
use proptest::prelude::*;
use tidefell_meta::{copy_id, Folder, Retention};
use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::{ContentHash, DeviceId, EntryId, FolderId, Signed};

const FOLDER: FolderId = FolderId([9; 32]);
const NOW: i64 = 1_000;

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn device(n: u8) -> DeviceId {
    DeviceId(key(n).verifying_key().to_bytes())
}

fn replica(n: u8) -> Folder {
    Folder::new(FOLDER, device(n), u64::from(n), Retention::Days(30))
}

/// What happens next in a history of three replicas.
#[derive(Debug, Clone)]
enum Step {
    /// Replica `r` writes entry `e` at path `p` with content `c`, or deletes it if `c` is 0.
    Change { r: u8, e: u8, p: u8, c: u8, t: i64 },
    /// Replica `to` receives every version replica `from` holds.
    Sync { from: u8, to: u8 },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0u8..3, 0u8..3, 0u8..3, 0u8..4, 0i64..50)
            .prop_map(|(r, e, p, c, t)| Step::Change { r, e, p, c, t }),
        1 => (0u8..3, 0u8..3).prop_map(|(from, to)| Step::Sync { from, to }),
    ]
}

fn every_version(folder: &Folder) -> Vec<Signed<EntryVersion>> {
    folder
        .index_since(folder.epoch(), 0, usize::MAX)
        .0
        .into_iter()
        .map(|s| (**s.signed.as_ref().expect("never saved, so held")).clone())
        .collect()
}

/// Runs a history and returns every version it made.
fn run(steps: &[Step]) -> Vec<Signed<EntryVersion>> {
    let mut replicas = [replica(1), replica(2), replica(3)];
    let mut made = Vec::new();
    for step in steps {
        match *step {
            Step::Change { r, e, p, c, t } => {
                let kind = if c == 0 {
                    EntryKind::Tombstone
                } else {
                    EntryKind::File {
                        content: ContentHash([c; 32]),
                        size: u64::from(c),
                        executable: false,
                    }
                };
                let path = format!("f{p}");
                let r = usize::from(r);
                // A change the folder refuses, such as deleting what it does not have, is
                // simply not made.
                if let Ok(v) =
                    replicas[r].local_change(&key(r as u8 + 1), EntryId([e; 16]), &path, kind, t)
                {
                    made.push(v);
                }
            }
            Step::Sync { from, to } if from != to => {
                for v in every_version(&replicas[usize::from(from)]) {
                    replicas[usize::from(to)].apply_remote(v, NOW).unwrap();
                }
            }
            Step::Sync { .. } => {}
        }
    }
    made
}

/// What must be the same on every replica: each entry's winner and conflict copies.
type State = BTreeMap<EntryId, (Vec<u8>, Vec<Vec<u8>>)>;

fn body(v: &EntryVersion) -> Vec<u8> {
    postcard::to_stdvec(v).unwrap()
}

fn state(folder: &Folder) -> State {
    folder
        .entries()
        .map(|v| v.to_version())
        .map(|current| {
            let mut conflicts: Vec<Vec<u8>> = folder
                .conflicts(&current.entry)
                .iter()
                .map(|s| body(&s.version))
                .collect();
            conflicts.sort();
            (current.entry, (body(&current), conflicts))
        })
        .collect()
}

/// What the recycle bin may hold, whatever the order: versions that were made, a deleted
/// entry's last live version while the entry stays deleted, and versions that lost a decision,
/// kept under the ID derived from their signed form, while they are neither the winner nor an
/// open conflict.
fn check_recycle_bin(folder: &Folder, made: &[Signed<EntryVersion>]) -> Result<(), TestCaseError> {
    let made: Vec<(EntryId, EntryVersion)> = made
        .iter()
        .map(|s| (copy_id(s), s.verify().unwrap()))
        .collect();
    for item in folder.recycle_bin().iter() {
        match item.conflict_of {
            None => {
                prop_assert!(
                    made.iter().any(|(_, v)| *v == item.version),
                    "not made: {item:?}"
                );
                prop_assert!(
                    matches!(
                        folder.current(&item.version.entry).map(|v| v.kind),
                        Some(EntryKind::Tombstone)
                    ),
                    "recycled while live: {item:?}"
                );
            }
            Some(of) => {
                let (_, lost) = made
                    .iter()
                    .find(|(id, _)| *id == item.version.entry)
                    .ok_or_else(|| TestCaseError::fail(format!("no version for {item:?}")))?;
                let mut kept = lost.clone();
                kept.entry = item.version.entry;
                prop_assert_eq!(&kept, &item.version);
                prop_assert_eq!(lost.entry, of);
                prop_assert!(
                    folder.current(&of).map(|v| v.to_version()).as_ref() != Some(lost),
                    "the winner was recycled"
                );
                prop_assert!(
                    folder.open_conflicts(&of).all(|s| s.version != *lost),
                    "an open conflict was recycled"
                );
            }
        }
    }
    Ok(())
}

fn apply_all(
    n: u8,
    versions: &[Signed<EntryVersion>],
    made: &[Signed<EntryVersion>],
) -> Result<State, TestCaseError> {
    let mut folder = replica(n);
    for v in versions {
        folder.apply_remote(v.clone(), NOW).unwrap();
    }
    check_recycle_bin(&folder, made)?;
    Ok(state(&folder))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn replicas_converge_whatever_the_order(
        (made, shuffled) in prop::collection::vec(step(), 1..30)
            .prop_map(|steps| run(&steps))
            .prop_flat_map(|made| (Just(made.clone()), Just(made).prop_shuffle())),
    ) {
        let in_order = apply_all(10, &made, &made)?;
        let reversed: Vec<_> = made.iter().rev().cloned().collect();
        prop_assert_eq!(&apply_all(11, &reversed, &made)?, &in_order);
        prop_assert_eq!(&apply_all(12, &shuffled, &made)?, &in_order);
    }
}
