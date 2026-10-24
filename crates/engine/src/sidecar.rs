//! A small JSON file next to each file the engine keeps in a folder's reserved directory, saying
//! what the file is (ADR 0034), so a folder whose metadata is lost or damaged gets its recycle
//! bin and history back when it is rebuilt.
//!
//! Each is `<file>.json`: for deleted content kept for the recycle bin, the recycle-bin items with
//! that content; for a previous version, its entry and record; for a file removed from this
//! device only, its entry and record. Records holding version vectors, which JSON cannot key by
//! device, are kept exactly as their postcard encoding in hex, beside a readable path. A sidecar is written when its file is kept and removed
//! with it. Failing to write one costs only what a rebuild could restore, so it never fails the
//! operation.

use std::fs;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use shoal_meta::Recycled;
use shoal_proto::sync::EntryKind;
use shoal_proto::{ContentHash, EntryId};
use tracing::debug;

use crate::history::Previous;
use crate::state::{FolderState, SetAside};

/// The layout of the records below.
const FORMAT: u32 = 1;

/// What a file in the kept directory holds: deleted content, with the recycle-bin items that
/// have it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Kept {
    pub(crate) format: u32,
    pub(crate) content: ContentHash,
    pub(crate) recycled: Vec<Item>,
}

/// A recycle-bin item in a kept file's sidecar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Item {
    pub(crate) entry: EntryId,
    pub(crate) path: String,
    pub(crate) deleted_at_ms: i64,
    /// The item exactly, postcard in hex.
    pub(crate) record: String,
}

impl Item {
    /// The recycle-bin item, if the record decodes.
    pub(crate) fn recycled(&self) -> Option<Recycled> {
        postcard::from_bytes(&from_hex(&self.record)?).ok()
    }
}

/// What a file in the history directory is: a previous version of `entry`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct History {
    pub(crate) format: u32,
    pub(crate) entry: EntryId,
    pub(crate) previous: Previous,
}

/// What a file set aside on this device only is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Aside {
    pub(crate) format: u32,
    pub(crate) entry: EntryId,
    pub(crate) path: String,
    /// The [`SetAside`] record exactly, postcard in hex.
    pub(crate) record: String,
}

impl Aside {
    /// The set-aside record, if it decodes.
    pub(crate) fn set_aside(&self) -> Option<SetAside> {
        postcard::from_bytes(&from_hex(&self.record)?).ok()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The sidecar of `file`.
pub(crate) fn of(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(".json");
    PathBuf::from(name)
}

/// Writes the sidecar of `file`.
pub(crate) fn write<T: Serialize>(file: &Path, record: &T) {
    let path = of(file);
    let written = serde_json::to_vec_pretty(record)
        .map_err(std::io::Error::other)
        .and_then(|json| {
            let tmp = path.with_extension("json.tmp");
            fs::write(&tmp, json)?;
            crate::rename_durably(&tmp, &path)
        });
    if let Err(e) = written {
        debug!(path = %path.display(), error = %e, "cannot write a sidecar");
    }
}

/// Reads the sidecar of `file`, if it has a readable one.
pub(crate) fn read<T: DeserializeOwned>(file: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(of(file)).ok()?).ok()
}

/// Removes the sidecar of `file`, if any.
pub(crate) fn remove(file: &Path) {
    let _ = fs::remove_file(of(file));
}

/// The recycle-bin items with `content`, as a kept file's sidecar holds them.
pub(crate) fn kept(state: &FolderState, content: ContentHash) -> Kept {
    let mut recycled: Vec<Item> = state
        .meta
        .recycle_bin()
        .iter()
        .filter(
            |item| matches!(item.version.kind, EntryKind::File { content: c, .. } if c == content),
        )
        .map(|item| Item {
            entry: item.version.entry,
            path: item.version.path.clone(),
            deleted_at_ms: item.deleted_at_ms,
            record: hex(&crate::tracked::encode(item)),
        })
        .collect();
    recycled.sort_by_key(|item| item.entry);
    Kept {
        format: FORMAT,
        content,
        recycled,
    }
}

/// The sidecar of a previous version of `entry`.
pub(crate) fn history(entry: EntryId, previous: &Previous) -> History {
    History {
        format: FORMAT,
        entry,
        previous: previous.clone(),
    }
}

/// The sidecar of a file of `entry` set aside on this device.
pub(crate) fn aside(entry: EntryId, set_aside: &SetAside) -> Aside {
    Aside {
        format: FORMAT,
        entry,
        path: set_aside.path.clone(),
        record: hex(&crate::tracked::encode(set_aside)),
    }
}

/// Writes the sidecars of kept files whose recycle-bin items changed, and of any file that has
/// none, as files kept by earlier versions do not.
pub(crate) fn refresh(state: &FolderState) {
    let root = state.config.path.clone();
    let kept_dir = crate::content::kept_dir(&root);
    for (content, name) in state.kept.iter() {
        let file = kept_dir.join(name);
        let record = kept(state, *content);
        if read::<Kept>(&file).as_ref() != Some(&record) {
            write(&file, &record);
        }
    }
    let history_dir = crate::history::history_dir(&root);
    for (entry, versions) in state.history.iter() {
        for previous in versions {
            let file = history_dir.join(&previous.file);
            if !of(&file).exists() {
                write(&file, &history(*entry, previous));
            }
        }
    }
    for (entry, item) in state.set_aside.iter() {
        let file = crate::setaside::file_of(&root, item);
        if !of(&file).exists() {
            write(&file, &aside(*entry, item));
        }
    }
}

/// The files in `dir` with a readable sidecar, and what each is.
fn described<T: DeserializeOwned>(dir: &Path) -> Vec<(String, T)> {
    let Ok(read_dir) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, T)> = read_dir
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| !name.ends_with(".json") && !name.ends_with(".tmp"))
        .filter_map(|name| Some((name.clone(), read(&dir.join(&name))?)))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// Gives a folder being rebuilt back its recycle bin, history and files set aside on this
/// device, from the sidecars beside their files. A file without a readable sidecar is left for
/// cleanup; what it was cannot be known.
pub(crate) fn restore(state: &mut FolderState) {
    debug_assert!(state.rebuilding());
    let root = state.config.path.clone();
    let (mut items, mut previous, mut aside) = (0, 0, 0);
    for (name, kept) in described::<Kept>(&crate::content::kept_dir(&root)) {
        let recycled: Vec<Recycled> = kept.recycled.iter().filter_map(Item::recycled).collect();
        // Kept too with no item in the bin, as for a version that lost a conflict: the peers'
        // versions say whether it is still needed once they arrive.
        state.kept.insert(kept.content, name);
        for item in recycled {
            state.meta.restore_recycled(item);
            items += 1;
        }
    }
    for (name, history) in described::<History>(&crate::history::history_dir(&root)) {
        if history.previous.file != name {
            continue;
        }
        let mut versions = state
            .history
            .get(&history.entry)
            .cloned()
            .unwrap_or_default();
        versions.push(history.previous);
        versions.sort_by_key(|p| p.id);
        state.history.insert(history.entry, versions);
        previous += 1;
    }
    for (name, record) in described::<Aside>(&crate::setaside::recycle_dir(&root)) {
        if let Some(item) = record.set_aside().filter(|item| item.file == name) {
            state.set_aside.insert(record.entry, item);
            aside += 1;
        }
    }
    tracing::info!(
        folder = %state.config.id,
        recycled = items,
        history = previous,
        set_aside = aside,
        "restored from sidecars"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_read_back_exactly() {
        let bytes = vec![0, 1, 0x7f, 0xff];
        assert_eq!(from_hex(&hex(&bytes)), Some(bytes));
        assert_eq!(from_hex("zz"), None);
        let item = Item {
            entry: EntryId([1; 16]),
            path: "a.txt".into(),
            deleted_at_ms: 5,
            record: "00".into(),
        };
        assert!(item.recycled().is_none(), "not a recycle-bin item");
        let aside = Aside {
            format: FORMAT,
            entry: EntryId([1; 16]),
            path: "a.txt".into(),
            record: String::new(),
        };
        assert!(aside.set_aside().is_none());
    }
}
