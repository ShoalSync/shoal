//! Folder roots and their marker (ADR 0037).
//!
//! A folder's root holds `.tidefell/folder`, naming the folder and the device that created it.
//! The root is created only when the folder is created or joined. Afterwards, a missing root
//! or marker makes the folder unavailable: no scans, writes or deletes until it is back. An
//! unmounted drive therefore looks like a folder that is away, not one whose files were all
//! deleted.

use std::fs;
use std::path::{Path, PathBuf};

use tidefell_proto::{DeviceId, FolderId};

use crate::scan::RESERVED_DIR;
use crate::state::FolderState;
use crate::{io_err, Error, Result};

const HEADER: &str =
    "# Tidefell folder root. Removing this file stops Tidefell syncing this folder.";

fn path(root: &Path) -> PathBuf {
    root.join(RESERVED_DIR).join("folder")
}

/// The folder ID in `root`'s marker, `Ok(None)` if it has none.
fn read(root: &Path) -> std::io::Result<Option<String>> {
    match fs::read_to_string(path(root)) {
        Ok(text) => Ok(Some(
            text.lines()
                .find_map(|l| l.strip_prefix("folder = "))
                .unwrap_or_default()
                .trim()
                .to_owned(),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn write(root: &Path, folder: FolderId, device: DeviceId) -> Result<()> {
    let marker = path(root);
    let dir = root.join(RESERVED_DIR);
    fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    hide(&dir);
    let tmp = dir.join("folder.tmp");
    fs::write(
        &tmp,
        format!("{HEADER}\nfolder = {folder}\ndevice = {device}\n"),
    )
    .map_err(io_err(&tmp))?;
    crate::rename_durably(&tmp, &marker).map_err(io_err(&marker))
}

/// Hides the reserved directory, and the marker in it, from Explorer's listings, as the leading
/// dot does elsewhere (ADR 0042). Without unsafe code that is `attrib`'s job, which keeps the
/// directory's other attributes; a directory it cannot hide still works, only visibly.
#[cfg(windows)]
fn hide(dir: &Path) {
    let attrib = std::env::var_os("SystemRoot")
        .map(|root| Path::new(&root).join("System32").join("attrib.exe"))
        .unwrap_or_else(|| PathBuf::from("attrib.exe"));
    let hidden = std::process::Command::new(attrib)
        .arg("+H")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !hidden.is_ok_and(|s| s.success()) {
        tracing::warn!(dir = %dir.display(), "cannot hide the reserved directory");
    }
}

#[cfg(not(windows))]
fn hide(_dir: &Path) {}

/// Creates `root` for a folder being created or joined, and marks it as the folder's.
pub(crate) fn create(root: &Path, folder: FolderId, device: DeviceId) -> Result<()> {
    fs::create_dir_all(root).map_err(io_err(root))?;
    match read(root).map_err(io_err(path(root)))? {
        Some(id) if id == folder.to_string() => Ok(()),
        Some(id) => Err(Error::Invalid(format!(
            "{} already holds folder {id}",
            root.display()
        ))),
        None => write(root, folder, device),
    }
}

/// Why `root` cannot be used for `folder` now, if it cannot.
pub(crate) fn check(root: &Path, folder: FolderId) -> Option<String> {
    match fs::metadata(root) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Some(format!("{} is not a directory", root.display())),
        Err(_) => {
            return Some(format!(
                "{} is missing; is its drive mounted?",
                root.display()
            ))
        }
    }
    match read(root) {
        Ok(Some(id)) if id == folder.to_string() => None,
        Ok(Some(id)) => Some(format!(
            "{} holds folder {id}, not this one",
            root.display()
        )),
        Ok(None) => Some(format!(
            "{} has no {RESERVED_DIR}/folder marker; is its drive mounted?",
            root.display()
        )),
        Err(e) => Some(format!("cannot read {}: {e}", path(root).display())),
    }
}

/// Marks the root of a folder created before markers existed, if it is plainly the folder's:
/// nothing was ever recorded on disk, or a file recorded there still exists. Otherwise the
/// folder stays unavailable until its user checks the path.
pub(crate) fn adopt(state: &FolderState, device: DeviceId) -> Result<()> {
    let root = &state.config.path;
    if !root.is_dir() || path(root).exists() {
        return Ok(());
    }
    let known = state.local.is_empty() || state.local.keys().any(|p| root.join(&**p).exists());
    if known {
        write(root, state.config.id, device)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (FolderId, DeviceId) {
        (FolderId([1; 32]), DeviceId([2; 32]))
    }

    #[test]
    fn a_created_root_passes_the_check() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("photos");
        let (folder, device) = ids();
        create(&root, folder, device).unwrap();
        assert_eq!(check(&root, folder), None);
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
            let attributes = fs::metadata(root.join(RESERVED_DIR))
                .unwrap()
                .file_attributes();
            assert_ne!(attributes & FILE_ATTRIBUTE_HIDDEN, 0, "{attributes:#x}");
        }
        // Creating again, as when a folder is removed and joined again, is fine.
        create(&root, folder, device).unwrap();
    }

    #[test]
    fn a_missing_root_or_marker_fails_the_check() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("photos");
        let (folder, _) = ids();
        assert!(check(&root, folder).unwrap().contains("is missing"));
        fs::create_dir(&root).unwrap();
        assert!(check(&root, folder)
            .unwrap()
            .contains("no .tidefell/folder"));
    }

    #[test]
    fn another_folders_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (folder, device) = ids();
        create(dir.path(), folder, device).unwrap();
        let other = FolderId([3; 32]);
        assert!(check(dir.path(), other).unwrap().contains("holds folder"));
        assert!(matches!(
            create(dir.path(), other, device),
            Err(Error::Invalid(_))
        ));
    }
}
