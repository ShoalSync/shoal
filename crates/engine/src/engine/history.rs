//! A file's versions: the current one and those kept on this device (ADR 0029).

use std::path::PathBuf;

use tidefell_proto::sync::EntryKind;
use tidefell_proto::FolderId;

use super::Engine;
use crate::history::history_dir;
use crate::views::{VersionInfo, VersionKind};
use crate::{Error, Result};

const DAY_MS: i64 = 86_400_000;

impl Engine {
    /// The versions of the live file at `path`: the current one first, then those that
    /// conflict with it, then the previous versions kept on this device, newest first.
    pub fn versions(&self, folder: FolderId, path: &str) -> Result<Vec<VersionInfo>> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let current = state
            .meta
            .find_path(path)
            .filter(|v| matches!(v.kind, EntryKind::File { .. }))
            .ok_or_else(|| Error::NoSuchFile(path.to_owned()))?;
        let EntryKind::File { size, .. } = current.kind else {
            unreachable!("filtered to files");
        };
        let days = state.meta.recycle_bin().retention().days();
        let mut out = vec![VersionInfo {
            id: "current".into(),
            kind: VersionKind::Current,
            size,
            modified_ms: current.modified_ms,
            author: Some(current.author),
            author_name: Some(self.device_name(&state, &current.author)),
            kept_ms: None,
            expires_ms: None,
        }];
        let mut previous: Vec<_> = state
            .history
            .get(&current.entry)
            .into_iter()
            .flatten()
            .collect();
        previous.sort_by_key(|v| std::cmp::Reverse(v.kept_ms));
        out.extend(previous.into_iter().map(|v| VersionInfo {
            id: format!("previous:{:016x}", v.id),
            kind: VersionKind::Previous,
            size: v.size,
            modified_ms: v.modified_ms,
            author: None,
            author_name: None,
            kept_ms: Some(v.kept_ms),
            expires_ms: days.map(|d| v.kept_ms + i64::from(d) * DAY_MS),
        }));
        // Versions waiting for a decision come right after the current one (ADR 0029).
        let conflicts = state.meta.open_conflicts(&current.entry).filter_map(|s| {
            let EntryKind::File { size, .. } = s.version.kind else {
                return None;
            };
            Some(VersionInfo {
                id: super::conflicts::conflict_version_id(s),
                kind: VersionKind::Conflict,
                size,
                modified_ms: s.version.modified_ms,
                author: Some(s.version.author),
                author_name: Some(self.device_name(&state, &s.version.author)),
                kept_ms: None,
                expires_ms: None,
            })
        });
        let conflicts: Vec<VersionInfo> = conflicts.collect();
        out.splice(1..1, conflicts);
        Ok(out)
    }

    /// The file holding a previous version of the live file at `path`, to open read-only.
    pub fn version_file(&self, folder: FolderId, path: &str, version: &str) -> Result<PathBuf> {
        let handle = self.inner.folder(&folder)?;
        let state = handle.lock();
        let entry = state
            .meta
            .find_path(path)
            .map(|v| v.entry)
            .ok_or_else(|| Error::NoSuchFile(path.to_owned()))?;
        let id = version
            .strip_prefix("previous:")
            .and_then(|id| u64::from_str_radix(id, 16).ok())
            .ok_or_else(|| Error::Invalid(format!("not a previous version: {version:?}")))?;
        let kept = state
            .history
            .get(&entry)
            .into_iter()
            .flatten()
            .find(|v| v.id == id)
            .ok_or_else(|| Error::Invalid(format!("no version {version} of {path}")))?;
        Ok(history_dir(&state.config.path).join(&kept.file))
    }

    /// Makes a previous version the file's content again, as a new version that syncs to every
    /// device like any edit. The content it replaces becomes a previous version in turn.
    pub async fn restore_version(&self, folder: FolderId, path: &str, version: &str) -> Result<()> {
        let kept = self.version_file(folder, path, version)?;
        let tmp_dir = self
            .inner
            .folder(&folder)?
            .lock()
            .config
            .path
            .join(crate::scan::RESERVED_DIR)
            .join("tmp");
        std::fs::create_dir_all(&tmp_dir).map_err(crate::io_err(&tmp_dir))?;
        let copy = tmp_dir.join(format!(
            "restore-{:016x}",
            u64::from_le_bytes(crate::random_bytes()?)
        ));
        std::fs::copy(&kept, &copy).map_err(crate::io_err(&kept))?;
        self.write(folder, path, &copy).await
    }
}
