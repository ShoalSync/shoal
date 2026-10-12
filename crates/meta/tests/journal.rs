//! Saving only what changed (ADR 0034): a folder rebuilt from what its journals stored is the
//! folder it was, whatever happened in between and whenever it was saved.

use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;
use proptest::prelude::*;
use tidefell_meta::{Folder, Header, Journal, Recycled, Retention};
use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::{ContentHash, DeviceId, EntryId, FolderId, Signed};

const FOLDER: FolderId = FolderId([9; 32]);

fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn device(n: u8) -> DeviceId {
    DeviceId(key(n).verifying_key().to_bytes())
}

/// What a database holds for a folder: its header, the versions it keeps by sequence, and its
/// recycle bin.
#[derive(Default)]
struct Records {
    header: Option<Header>,
    versions: BTreeMap<u64, Signed<EntryVersion>>,
    recycle: BTreeMap<EntryId, Recycled>,
}

impl Records {
    fn save(&mut self, folder: &mut Folder) {
        let journal = folder.take_journal();
        self.apply(folder.header(), journal);
    }

    fn apply(&mut self, header: Header, journal: Journal) {
        for (sequence, signed) in journal.stored {
            assert!(
                self.versions.insert(sequence, signed).is_none(),
                "a sequence stored twice"
            );
        }
        for sequence in journal.dropped {
            assert!(
                self.versions.remove(&sequence).is_some(),
                "dropped what was not stored"
            );
        }
        for (entry, item) in journal.recycle {
            match item {
                Some(item) => self.recycle.insert(entry, item),
                None => self.recycle.remove(&entry),
            };
        }
        self.header = Some(header);
    }

    fn load(&self) -> Folder {
        Folder::from_records(
            self.header.unwrap(),
            self.versions.iter().map(|(s, v)| (*s, v.clone())),
            self.recycle.values().cloned(),
        )
        .unwrap()
    }
}

#[derive(Debug, Clone)]
enum Step {
    /// Replica `r` writes entry `e` at path `p` with content `c`, or deletes it if `c` is 0.
    Change { r: u8, e: u8, p: u8, c: u8, t: i64 },
    /// Replica `to` receives every version replica `from` holds.
    Sync { from: u8, to: u8 },
    /// Replica `r` saves what changed.
    Save { r: u8 },
    /// Replica `r` discards entry `e` from its recycle bin.
    Discard { r: u8, e: u8 },
    /// Replica `r` purges its recycle bin at a late time, or empties it.
    Purge { r: u8, all: bool },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        4 => (0u8..3, 0u8..3, 0u8..3, 0u8..4, 0i64..50)
            .prop_map(|(r, e, p, c, t)| Step::Change { r, e, p, c, t }),
        2 => (0u8..3, 0u8..3).prop_map(|(from, to)| Step::Sync { from, to }),
        2 => (0u8..3).prop_map(|r| Step::Save { r }),
        1 => (0u8..3, 0u8..3).prop_map(|(r, e)| Step::Discard { r, e }),
        1 => (0u8..3, any::<bool>()).prop_map(|(r, all)| Step::Purge { r, all }),
    ]
}

/// Every version `folder` holds, signed: from memory until saved, then from `records`, as an
/// engine reads them from its database (ADR 0035).
fn every_version(folder: &Folder, records: &Records) -> Vec<Signed<EntryVersion>> {
    folder
        .index_since(folder.epoch(), 0, usize::MAX)
        .0
        .into_iter()
        .map(|s| match &s.signed {
            Some(signed) => (**signed).clone(),
            None => records.versions[&s.sequence].clone(),
        })
        .collect()
}

/// Everything about a folder a rebuild must keep.
fn everything(folder: &Folder) -> String {
    let mut versions: Vec<(u64, Vec<u8>)> = folder
        .index_since(folder.epoch(), 0, usize::MAX)
        .0
        .into_iter()
        .map(|s| (s.sequence, s.id.0.to_vec()))
        .collect();
    versions.sort();
    let current: BTreeMap<EntryId, Vec<u8>> = folder
        .entries()
        .map(|v| v.to_version())
        .map(|v| (v.entry, postcard::to_stdvec(&v).unwrap()))
        .collect();
    let conflicts: BTreeMap<EntryId, Vec<u64>> = folder
        .entries()
        .map(|v| v.to_version())
        .map(|v| {
            (
                v.entry,
                folder
                    .conflicts(&v.entry)
                    .iter()
                    .map(|s| s.sequence)
                    .collect(),
            )
        })
        .collect();
    let mut recycled: Vec<String> = folder
        .recycle_bin()
        .iter()
        .map(|i| format!("{i:?}"))
        .collect();
    recycled.sort();
    format!(
        "{:?}\n{versions:?}\n{current:?}\n{conflicts:?}\n{recycled:?}",
        folder.header()
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn a_folder_rebuilt_from_its_journals_is_the_same(
        steps in prop::collection::vec(step(), 1..40),
    ) {
        let mut replicas: Vec<Folder> = (1..=3)
            .map(|n| Folder::new(FOLDER, device(n), u64::from(n), Retention::Days(30)))
            .collect();
        let mut records: Vec<Records> = (0..3).map(|_| Records::default()).collect();
        for step in &steps {
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
                    let r = usize::from(r);
                    let _ = replicas[r].local_change(
                        &key(r as u8 + 1),
                        EntryId([e; 16]),
                        &format!("f{p}"),
                        kind,
                        t,
                    );
                }
                Step::Sync { from, to } if from != to => {
                    let from = usize::from(from);
                    for v in every_version(&replicas[from], &records[from]) {
                        replicas[usize::from(to)].apply_remote(v, 1_000).unwrap();
                    }
                }
                Step::Sync { .. } => {}
                Step::Save { r } => {
                    let r = usize::from(r);
                    records[r].save(&mut replicas[r]);
                }
                Step::Discard { r, e } => {
                    replicas[usize::from(r)].discard(&EntryId([e; 16]));
                }
                Step::Purge { r, all } => {
                    let r = &mut replicas[usize::from(r)];
                    if all {
                        r.discard_all();
                    } else {
                        r.purge_expired(i64::MAX);
                    }
                }
            }
        }
        for (replica, records) in replicas.iter_mut().zip(&mut records) {
            records.save(replica);
            prop_assert_eq!(everything(&records.load()), everything(replica));
            // Everything is saved: a full journal has nothing more, and what was saved is
            // every version the replica keeps.
            prop_assert!(replica.full_journal().stored.is_empty());
            let saved: Vec<(u64, EntryId)> = records
                .versions
                .iter()
                .map(|(s, v)| (*s, tidefell_meta::copy_id(v)))
                .collect();
            prop_assert_eq!(saved, replica.stored_ids());
        }
    }
}
