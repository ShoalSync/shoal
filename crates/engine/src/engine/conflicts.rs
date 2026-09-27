//! Conflicts that wait for the user to decide (ADR 0029).

use std::path::PathBuf;

use tidefell_meta::{copy_id, StoredVersion};
use tidefell_proto::sync::{EntryKind, EntryVersion};
use tidefell_proto::{DeviceId, EntryId, FolderId, VersionVector};

use super::Engine;
use crate::state::FolderState;
use crate::views::Event;
use crate::{now_ms, Error, Result};

/// What to keep when resolving a conflict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Keep one version, `current` or `conflict:<id>`, as the file's content. The others go to
    /// every device's recycle bin.
    Keep(String),
    /// Keep the current version as the file, and each other version as a file of its own
    /// beside it, named `<stem> (<device>, <date>)<extension>`.
    KeepBoth,
}

/// The ID of a conflicting version in [`crate::VersionInfo`].
pub(crate) fn conflict_version_id(stored: &StoredVersion) -> String {
    format!("conflict:{}", copy_id(&stored.signed))
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `<stem> (<device>, <date>)<extension>` beside `path`, with a number added if that is taken,
/// for a version of it made by `device` at `modified_ms` (UTC date).
pub(crate) fn copy_path(
    state: &FolderState,
    path: &str,
    device: &str,
    modified_ms: i64,
) -> Option<String> {
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name),
        None => (String::new(), path),
    };
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    let (y, m, d) = civil_from_days(modified_ms.div_euclid(86_400_000));
    // Characters a path may not hold are left out of the device's name.
    let device: String = device
        .chars()
        .filter(|c| !matches!(c, '/' | '\\') && !c.is_control())
        .collect();
    let label = format!("{device}, {y:04}-{m:02}-{d:02}");
    let taken = |candidate: &str| {
        let below = format!("{candidate}/");
        state.meta.find_path(candidate).is_some()
            || state.meta.layout().entry_at(candidate).is_some()
            || state
                .meta
                .entries()
                .any(|v| v.path.starts_with(&below) && !matches!(v.kind, EntryKind::Tombstone))
            || state.config.path.join(candidate).symlink_metadata().is_ok()
    };
    (1..1000)
        .map(|n| {
            if n == 1 {
                format!("{dir}{stem} ({label}){ext}")
            } else {
                format!("{dir}{stem} ({label}, {n}){ext}")
            }
        })
        .find(|c| tidefell_proto::sync::is_valid_path(c) && !taken(c))
}

impl Engine {
    /// A device's name: this device's, a connected peer's, one learned from a confirmed copy,
    /// or the start of its ID.
    pub(crate) fn device_name(&self, state: &FolderState, device: &DeviceId) -> String {
        if *device == self.id() {
            return self
                .inner
                .certificate
                .verify()
                .map(|c| c.name)
                .unwrap_or_default();
        }
        self.node
            .peers()
            .into_iter()
            .find(|p| p.device() == *device)
            .map(|p| p.certificate().name.clone())
            .or_else(|| state.copy_names.get(device).cloned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| device.to_string().chars().take(8).collect())
    }

    /// The versions of the file at `path` that wait for a decision besides the current one.
    fn open_conflicts(
        state: &FolderState,
        path: &str,
    ) -> Result<(EntryVersion, Vec<StoredVersion>)> {
        let current = state
            .meta
            .find_path(path)
            .filter(|v| matches!(v.kind, EntryKind::File { .. }))
            .cloned()
            .ok_or_else(|| Error::NoSuchFile(path.to_owned()))?;
        let open = state.meta.open_conflicts(&current.entry).cloned().collect();
        Ok((current, open))
    }

    /// Writes a read-only copy of a version of the file at `path` and returns where: a previous
    /// version kept on this device, or a conflicting version, fetched from peers if it is not
    /// here (ADR 0029). The copy is in the folder's reserved directory; the caller may delete it.
    pub async fn open_version(
        &self,
        folder: FolderId,
        path: &str,
        version: &str,
    ) -> Result<PathBuf> {
        if version.starts_with("previous:") {
            return self.version_file(folder, path, version);
        }
        let handle = self.inner.folder(&folder)?;
        let (stored, dest) = {
            let state = handle.lock();
            let (_, open) = Self::open_conflicts(&state, path)?;
            let stored = open
                .into_iter()
                .find(|s| conflict_version_id(s) == version)
                .ok_or_else(|| Error::Invalid(format!("no version {version} of {path}")))?;
            let dir = state
                .config
                .path
                .join(crate::scan::RESERVED_DIR)
                .join("tmp");
            std::fs::create_dir_all(&dir).map_err(crate::io_err(&dir))?;
            let dest = dir.join(format!("version-{}", copy_id(&stored.signed)));
            (stored, dest)
        };
        if dest.exists() {
            return Ok(dest);
        }
        if self.inner.is_paused(&folder) {
            return Err(Error::Paused(folder));
        }
        if !crate::materialize::write_copy(
            &self.inner,
            &self.node,
            &handle,
            folder,
            &stored.version,
            &dest,
        )
        .await?
        {
            return Err(Error::Unavailable(path.to_owned()));
        }
        Ok(dest)
    }

    /// Decides a conflict on the file at `path` (ADR 0029). The decision is a new version that
    /// supersedes every conflicting version and syncs to every device. Versions not kept go to
    /// every device's recycle bin; with [`Resolution::KeepBoth`] they become files of their own.
    pub async fn resolve(
        &self,
        folder: FolderId,
        path: &str,
        resolution: Resolution,
    ) -> Result<()> {
        let handle = self.inner.folder(&folder)?;
        {
            let _guard = handle.disk().await?;
            let mut state = handle.lock();
            let (current, open) = Self::open_conflicts(&state, path)?;
            if open.is_empty() {
                return Err(Error::Invalid(format!("{path} has no conflict")));
            }
            let key = self.inner.key.signing_key();
            let keep = match &resolution {
                Resolution::Keep(id) if id == "current" => current.clone(),
                Resolution::Keep(id) => open
                    .iter()
                    .find(|s| conflict_version_id(s) == *id)
                    .map(|s| s.version.clone())
                    .ok_or_else(|| Error::Invalid(format!("no version {id} of {path}")))?,
                Resolution::KeepBoth => {
                    for other in &open {
                        let name = self.device_name(&state, &other.version.author);
                        let copy = copy_path(&state, path, &name, other.version.modified_ms)
                            .ok_or_else(|| Error::Invalid(format!("no free name beside {path}")))?;
                        state.meta.local_change_from(
                            key,
                            copy_id(&other.signed),
                            &VersionVector::new(),
                            &copy,
                            other.version.kind.clone(),
                            other.version.modified_ms,
                        )?;
                    }
                    current.clone()
                }
            };
            let all = state.meta.known_vector(&current.entry);
            let (_, vector) = state.meta.local_change_from(
                key,
                current.entry,
                &all,
                path,
                keep.kind.clone(),
                keep.modified_ms,
            )?;
            // The file on disk is the version kept: a later edit builds on the decision.
            if keep.kind == current.kind {
                if let Some(mut local) = state.local.get(path).cloned() {
                    if local.entry == current.entry {
                        local.base = vector;
                        state.local.insert(path.to_owned(), local);
                    }
                }
            }
            state.save()?;
        }
        tracing::info!(folder = %folder, path, ?resolution, "conflict resolved");
        self.inner.emit(Event::FolderChanged(folder));
        self.inner.changed();
        Ok(())
    }

    /// Restores `entry` from the recycle bin when it is a version that lost a conflict: as a
    /// file of its own beside the file it was a version of.
    pub(crate) fn restore_copy(&self, state: &mut FolderState, entry: EntryId) -> Result<()> {
        let item = state
            .meta
            .recycle_bin()
            .get(&entry)
            .cloned()
            .ok_or(Error::NotRecycled(entry))?;
        let name = self.device_name(state, &item.version.author);
        let path = copy_path(state, &item.version.path, &name, item.version.modified_ms)
            .ok_or_else(|| Error::Invalid(format!("no free name beside {}", item.version.path)))?;
        state
            .meta
            .restore_copy(self.inner.key.signing_key(), entry, &path, now_ms())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_723), (2026, 9, 27));
    }
}
