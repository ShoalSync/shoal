//! Version history (ADR 0029): content the engine replaces on disk is kept on this device as
//! previous versions of its entry, by moving the file aside instead of copying it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tidefell_chunker::{chunk_reader, Params};
use tidefell_proto::{ContentHash, EntryId};

use crate::scan::{stat, RESERVED_DIR};
use crate::state::{FolderState, LocalFile};
use crate::{io_err, now_ms, random_bytes, Error, Result};

/// A day, in milliseconds.
const DAY_MS: i64 = 86_400_000;

/// A previous version of an entry, kept in the folder's history directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Previous {
    /// Identifies the version within its entry.
    pub(crate) id: u64,
    pub(crate) content: ContentHash,
    pub(crate) size: u64,
    pub(crate) executable: bool,
    /// The file's modification time when it was replaced.
    pub(crate) modified_ms: i64,
    /// When this device replaced it, by this device's clock. Retention counts from here.
    pub(crate) kept_ms: i64,
    /// Where the file was.
    pub(crate) path: String,
    /// Its name in the history directory.
    pub(crate) file: String,
}

/// Where previous versions are kept, inside the folder's reserved directory.
pub(crate) fn history_dir(root: &Path) -> PathBuf {
    root.join(RESERVED_DIR).join("history")
}

/// Records an interrupted [`swap_in`], so the next start can undo it.
fn marker(root: &Path) -> PathBuf {
    root.join(RESERVED_DIR).join("swap")
}

/// Puts `new` at `target` and the file that was at `target` at `aside`, with no moment where
/// `target` is missing: a scan in between would record a delete that every device applies.
/// All three paths are on the folder's filesystem.
pub(crate) fn swap_in(root: &Path, new: &Path, target: &Path, aside: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{renameat_with, RenameFlags, CWD};
        use rustix::io::Errno;
        match renameat_with(CWD, new, CWD, target, RenameFlags::EXCHANGE) {
            // `new` now holds the old file.
            Ok(()) => return crate::rename_durably(new, aside),
            // Not supported by this filesystem, such as FAT or Android's shared storage.
            Err(Errno::INVAL | Errno::NOSYS | Errno::OPNOTSUPP) => {}
            Err(e) => return Err(e.into()),
        }
    }
    // A second name keeps the old file while the new one replaces it.
    if fs::hard_link(target, aside).is_ok() {
        return crate::rename_durably(new, target);
    }
    // Neither works: two renames, and a marker so a crash between them is undone.
    let marker = marker(root);
    fs::write(
        &marker,
        format!("{}\n{}\n", target.display(), aside.display()),
    )?;
    fs::File::open(&marker)?.sync_all()?;
    crate::rename_durably(target, aside)?;
    crate::rename_durably(new, target)?;
    fs::remove_file(&marker)
}

/// Undoes a [`swap_in`] interrupted between its two renames, before the folder is scanned.
pub(crate) fn recover(root: &Path) {
    let marker = marker(root);
    let Ok(text) = fs::read_to_string(&marker) else {
        return;
    };
    let mut lines = text.lines();
    if let (Some(target), Some(aside)) = (lines.next(), lines.next()) {
        let (target, aside) = (Path::new(target), Path::new(aside));
        if !target.exists() && aside.exists() {
            let _ = crate::rename_durably(aside, target);
        }
    }
    let _ = fs::remove_file(&marker);
}

/// Removes from `versions` those past `retention_days` (none if `None`), and thins the rest:
/// every version kept in the last day, then the newest one per day. Returns what was removed.
pub(crate) fn thin(
    versions: &mut Vec<Previous>,
    now_ms: i64,
    retention_days: Option<u32>,
) -> Vec<Previous> {
    versions.sort_by_key(|v| std::cmp::Reverse(v.kept_ms));
    let mut kept_days = std::collections::HashSet::new();
    let mut removed = Vec::new();
    versions.retain(|v| {
        let age = now_ms - v.kept_ms;
        let expired = retention_days.is_some_and(|days| age >= i64::from(days) * DAY_MS);
        let keep = !expired && (age < DAY_MS || kept_days.insert(v.kept_ms.div_euclid(DAY_MS)));
        if !keep {
            removed.push(v.clone());
        }
        keep
    });
    removed
}

/// Moves the file at `path`, recorded as `recorded`, into history as a previous version of its
/// entry, and puts `new` in its place if given. `state.local` is left to the caller: a new
/// version of the same entry replaces the record, and a path being vacated removes it.
pub(crate) fn keep(
    state: &mut FolderState,
    path: &str,
    recorded: &LocalFile,
    new: Option<&Path>,
) -> Result<()> {
    let root = state.config.path.clone();
    let dir = history_dir(&root);
    fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    // Keeps Android's media scanner out of the engine's own files, before the first move.
    let nomedia = root.join(RESERVED_DIR).join(".nomedia");
    if !nomedia.exists() {
        fs::write(&nomedia, b"").map_err(io_err(&nomedia))?;
    }
    let id = u64::from_le_bytes(random_bytes()?);
    let file = format!("{}-{id:016x}", recorded.entry);
    let (target, aside) = (root.join(path), dir.join(&file));
    match new {
        Some(new) => swap_in(&root, new, &target, &aside).map_err(io_err(&target))?,
        None => crate::rename_durably(&target, &aside).map_err(io_err(&target))?,
    }
    let disk = stat(&aside).ok_or_else(|| Error::Io {
        path: aside.clone(),
        source: io::Error::other("kept version vanished"),
    })?;
    // An editor may have changed the file since it was recorded: keep what is really there.
    let content = if disk.size == recorded.size && disk.mtime_ns == recorded.mtime_ns {
        recorded.content
    } else {
        let reader = io::BufReader::new(fs::File::open(&aside).map_err(io_err(&aside))?);
        let manifest =
            chunk_reader(reader, Params::DEFAULT, |_, _| Ok(())).map_err(io_err(&aside))?;
        let content = manifest.content;
        state.add_manifest(manifest);
        content
    };
    let versions = state.history.entry(recorded.entry).or_default();
    versions.insert(
        0,
        Previous {
            id,
            content,
            size: disk.size,
            executable: disk.executable,
            modified_ms: (disk.mtime_ns / 1_000_000) as i64,
            kept_ms: now_ms(),
            path: path.to_owned(),
            file,
        },
    );
    Ok(())
}

/// Removes history past the folder's retention and thins the rest (see [`thin`]). A removed
/// version whose content something else still needs, such as a conflict copy or a recycle-bin
/// item, and that is on disk nowhere else, is handed to the kept files instead of deleted.
pub(crate) fn purge(state: &mut FolderState, now_ms: i64) {
    let days = state.meta.recycle_bin().retention().days();
    let mut removed = Vec::new();
    // Only entries whose history thins change, so only those are saved again (ADR 0034).
    let entries: Vec<EntryId> = state.history.keys().copied().collect();
    for entry in entries {
        let Some(mut versions) = state.history.get(&entry).cloned() else {
            continue;
        };
        let gone = thin(&mut versions, now_ms, days);
        if !gone.is_empty() {
            removed.extend(gone);
            state.history.insert(entry, versions);
        }
    }
    state.history.retain(|_, v| !v.is_empty());
    if removed.is_empty() {
        return;
    }
    let root = state.config.path.clone();
    let needed = state.referenced_contents();
    for version in removed {
        let path = history_dir(&root).join(&version.file);
        let elsewhere = state.local.paths_with(&version.content).next().is_some()
            || state.kept.contains_key(&version.content)
            || state
                .history
                .values()
                .flatten()
                .any(|v| v.content == version.content);
        if needed.contains(&version.content) && !elsewhere {
            let dir = crate::content::kept_dir(&root);
            let name = format!("{}-{:016x}", version.content, version.id);
            if fs::create_dir_all(&dir).is_ok()
                && crate::rename_durably(&path, dir.join(&name)).is_ok()
            {
                state.kept.insert(version.content, name);
                continue;
            }
        }
        let _ = fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(id: u64, kept_ms: i64) -> Previous {
        Previous {
            id,
            content: ContentHash([id as u8; 32]),
            size: 1,
            executable: false,
            modified_ms: 0,
            kept_ms,
            path: "f".into(),
            file: format!("v{id}"),
        }
    }

    #[test]
    fn thinning_keeps_the_last_day_then_one_a_day() {
        let now = 100 * DAY_MS + DAY_MS / 2;
        let mut versions = vec![
            version(1, now - 60_000),        // a minute ago
            version(2, now - 3_600_000),     // an hour ago
            version(3, now - DAY_MS + 1),    // just under a day ago
            version(4, 99 * DAY_MS + 1_000), // yesterday, early
            version(5, 99 * DAY_MS + 5_000), // yesterday, later: kept for that day
            version(6, 90 * DAY_MS),         // ten days ago
            version(7, 60 * DAY_MS),         // past 30 days' retention
        ];
        let removed = thin(&mut versions, now, Some(30));
        let kept: Vec<u64> = versions.iter().map(|v| v.id).collect();
        assert_eq!(kept, [1, 2, 3, 5, 6]);
        let mut removed: Vec<u64> = removed.iter().map(|v| v.id).collect();
        removed.sort();
        assert_eq!(removed, [4, 7]);
    }

    #[test]
    fn forever_keeps_one_a_day_for_good() {
        let now = 1000 * DAY_MS;
        let mut versions = vec![version(1, 10 * DAY_MS), version(2, 10 * DAY_MS + 5)];
        let removed = thin(&mut versions, now, None);
        assert_eq!(versions.iter().map(|v| v.id).collect::<Vec<_>>(), [2]);
        assert_eq!(removed[0].id, 1);
    }

    #[test]
    fn swapping_keeps_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(history_dir(root)).unwrap();
        let (new, target, aside) = (
            root.join(RESERVED_DIR).join("new"),
            root.join("file.txt"),
            history_dir(root).join("old"),
        );
        fs::write(&new, b"new").unwrap();
        fs::write(&target, b"old").unwrap();
        swap_in(root, &new, &target, &aside).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(fs::read(&aside).unwrap(), b"old");
        assert!(!new.exists());
    }

    #[test]
    fn an_interrupted_swap_is_undone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(history_dir(root)).unwrap();
        let (target, aside) = (root.join("file.txt"), history_dir(root).join("old"));
        // As if the process died after moving the file aside and before putting the new one.
        fs::write(&aside, b"old").unwrap();
        fs::write(
            marker(root),
            format!("{}\n{}\n", target.display(), aside.display()),
        )
        .unwrap();
        recover(root);
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert!(!marker(root).exists());
        // A swap that finished leaves nothing to undo.
        fs::write(&aside, b"older").unwrap();
        recover(root);
        assert_eq!(fs::read(&target).unwrap(), b"old");
    }

    #[test]
    fn purging_hands_still_needed_content_to_the_kept_files() {
        use std::time::Duration;

        use tidefell_identity::DeviceKey;
        use tidefell_meta::Retention;
        use tidefell_proto::sync::EntryKind;
        use tidefell_proto::{EntryId, FolderId, UserId};

        use crate::config::{FolderConfig, ReplicaMode};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("folder");
        let config = FolderConfig {
            id: FolderId([3; 32]),
            path: root.clone(),
            mode: ReplicaMode::Full,
            members: vec![UserId([4; 32])],
            retention: Retention::Days(30),
            scan_interval: Duration::from_secs(10),
            sync: Default::default(),
        };
        let key = DeviceKey::from_seed([5; 32]);
        let mut state = FolderState::open(tmp.path(), config, key.id()).unwrap();
        // Another entry's current version has this content and it is on disk nowhere else, as
        // for a file dropped from this device: its old version here must not be lost.
        let needed = ContentHash([7; 32]);
        state
            .meta
            .local_change(
                key.signing_key(),
                EntryId([1; 16]),
                "other.txt",
                EntryKind::File {
                    content: needed,
                    size: 6,
                    executable: false,
                },
                0,
            )
            .unwrap();
        fs::create_dir_all(history_dir(&root)).unwrap();
        fs::write(history_dir(&root).join("needed"), b"needed").unwrap();
        fs::write(history_dir(&root).join("unneeded"), b"spare").unwrap();
        let mut old = version(1, 0);
        old.content = needed;
        old.file = "needed".into();
        let mut spare = version(2, 0);
        spare.file = "unneeded".into();
        state.history.insert(EntryId([2; 16]), vec![old, spare]);

        purge(&mut state, 40 * DAY_MS);
        assert!(state.history.is_empty(), "both expired");
        let kept = state.kept.get(&needed).expect("handed to the kept files");
        assert_eq!(
            fs::read(crate::content::kept_dir(&root).join(kept)).unwrap(),
            b"needed"
        );
        assert!(!history_dir(&root).join("unneeded").exists());
    }
}
