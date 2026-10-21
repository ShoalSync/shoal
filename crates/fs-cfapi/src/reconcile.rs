//! Making the folder's placeholders match the engine's files (ADR 0042).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::Path;

use cloud_filter::placeholder::PinState;
use tracing::{debug, warn};

use crate::backend::{Backend, BackendError, FileEntry};
use crate::ops;

/// What one pass did, for the tests and the logs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reconciled {
    /// Placeholders made for files not on this device.
    pub made: usize,
    /// Placeholders pointed at a newer version.
    pub updated: usize,
    /// Placeholders removed, their files no longer live.
    pub removed: usize,
    /// Files the engine placed, made in-sync placeholders.
    pub in_sync: usize,
}

fn backend_error(e: BackendError) -> io::Error {
    io::Error::other(e.to_string())
}

/// One pass: every live file not on disk gets a placeholder, a placeholder of a changed file is
/// updated, a placeholder of a file no longer live is removed, a file the engine placed becomes
/// an in-sync placeholder, and folders become placeholder folders. Nothing under the reserved
/// directory is touched. A placeholder recorded but gone was deleted by the user, perhaps while
/// the provider was away: the engine's scan deletes it, and this pass leaves it gone.
pub fn reconcile(root: &Path, backend: &dyn Backend) -> io::Result<Reconciled> {
    let mut done = Reconciled::default();
    let files = backend.files().map_err(backend_error)?;
    let recorded: HashSet<String> = backend
        .placeholders()
        .map_err(backend_error)?
        .into_iter()
        .collect();
    let by_path: HashMap<&str, &FileEntry> = files.iter().map(|f| (f.path.as_str(), f)).collect();

    for file in &files {
        if ops::reserved(&file.path) {
            continue;
        }
        let abs = ops::to_abs(root, &file.path);
        let result = match fs::symlink_metadata(&abs) {
            // Gone though recorded: the user deleted it, and the scan records that.
            Err(e) if e.kind() == io::ErrorKind::NotFound && recorded.contains(&file.path) => {
                Ok(())
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                if file.on_disk {
                    Ok(())
                } else {
                    debug!(path = %file.path, "making a placeholder");
                    ops::create(root, file).and_then(|()| {
                        done.made += 1;
                        // Made, then recorded: never recorded but missing.
                        backend
                            .placeholder_made(&file.path, file.content)
                            .map_err(backend_error)
                    })
                }
            }
            Err(e) => Err(e),
            Ok(meta) if !meta.is_file() => Ok(()),
            Ok(meta) if ops::dehydrated(&meta) => {
                if file.on_disk {
                    // Freed by the user: the scan drops the file's record.
                    backend.scan_path(&file.path);
                    Ok(())
                } else {
                    placeholder(&abs, file, &recorded, backend, &mut done)
                }
            }
            Ok(_) if file.on_disk => on_disk(&abs, file, &mut done),
            // Data here but not recorded as on disk: a placeholder just filled, which a scan
            // records, or the user's own file there, which a scan also finds.
            Ok(_) => {
                if ops::identity(&abs).is_some() && ops::in_sync(&abs) {
                    backend.scan_path(&file.path);
                }
                Ok(())
            }
        };
        if let Err(e) = result {
            warn!(path = %file.path, error = %e, "cannot update the placeholder");
        }
    }

    // Placeholders of files no longer live, and folders not yet placeholders.
    let mut stack = vec![root.to_owned()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let abs = entry.path();
            let Some(rel) = ops::to_rel(root, &abs) else {
                continue;
            };
            if ops::reserved(&rel) {
                continue;
            }
            let Ok(meta) = fs::symlink_metadata(&abs) else {
                continue;
            };
            if meta.is_dir() {
                if ops::identity(&abs).is_none() {
                    if let Err(e) = ops::convert_dir(&abs) {
                        debug!(path = %rel, error = %e, "cannot make the folder a placeholder");
                    }
                }
                stack.push(abs);
            } else if meta.is_file() && ops::dehydrated(&meta) {
                // Only a placeholder of no live file goes: one of a file the engine has on disk
                // was freed, as by "Free up space", and the scan records it as a placeholder.
                if !by_path.contains_key(rel.as_str()) {
                    // Forgotten first: removed while recorded, it would read as deleted.
                    let removed = backend
                        .placeholder_gone(&rel)
                        .map_err(backend_error)
                        .and_then(|()| fs::remove_file(&abs));
                    match removed {
                        Ok(()) => done.removed += 1,
                        Err(e) => warn!(path = %rel, error = %e, "cannot remove the placeholder"),
                    }
                }
            }
        }
    }
    // Recorded, gone, and no longer live: nothing to delete, only to forget.
    for rel in &recorded {
        if !by_path.contains_key(rel.as_str())
            && fs::symlink_metadata(ops::to_abs(root, rel)).is_err()
        {
            let _ = backend.placeholder_gone(rel);
        }
    }
    Ok(done)
}

/// The dehydrated placeholder at `abs` of `file`, not on disk.
fn placeholder(
    abs: &Path,
    file: &FileEntry,
    recorded: &HashSet<String>,
    backend: &dyn Backend,
    done: &mut Reconciled,
) -> io::Result<()> {
    let Some((shown, pin)) = ops::identity(abs) else {
        // Another program's placeholder, not ours to change.
        return Ok(());
    };
    if shown != Some((file.entry, file.content)) {
        debug!(path = %file.path, "updating the placeholder");
        ops::update(abs, file)?;
        done.updated += 1;
        backend
            .placeholder_made(&file.path, file.content)
            .map_err(backend_error)?;
    } else if !recorded.contains(&file.path) {
        backend
            .placeholder_made(&file.path, file.content)
            .map_err(backend_error)?;
    }
    // Pinned through the engine: Windows fills a pinned placeholder through the provider.
    if file.pinned && pin != PinState::Pinned {
        debug!(path = %file.path, "pinning the placeholder");
        ops::set_pin(abs, true)?;
    }
    Ok(())
}

/// The file at `abs`, which the engine has on disk as `file`.
fn on_disk(abs: &Path, file: &FileEntry, done: &mut Reconciled) -> io::Result<()> {
    let identity = ops::identity(abs);
    let shows = identity
        .as_ref()
        .is_some_and(|(shown, _)| *shown == Some((file.entry, file.content)));
    if (!shows || !ops::in_sync(abs)) && {
        debug!(path = %file.path, "making the file in sync");
        ops::make_in_sync(abs, file)?
    } {
        done.in_sync += 1;
    }
    let pinned = identity.is_some_and(|(_, pin)| pin == PinState::Pinned);
    if file.pinned != pinned {
        ops::set_pin(abs, file.pinned)?;
    }
    Ok(())
}
