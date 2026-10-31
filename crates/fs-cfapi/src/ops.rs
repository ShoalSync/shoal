//! Placeholder operations on the folder's files, and how its paths and identities are written.

use std::fs;
use std::io;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use cloud_filter::metadata::Metadata;
use cloud_filter::placeholder::{ConvertOptions, PinOptions, PinState, Placeholder, UpdateOptions};
use cloud_filter::placeholder_create_info::PlaceholderCreateInfo;
use shoal_proto::{ContentHash, EntryId};

use crate::backend::FileEntry;

/// The folder's reserved directory, which the provider never touches: a placeholder moved into
/// it stays out of sync, so Windows never frees it (ADR 0042).
pub(crate) const RESERVED_DIR: &str = ".shoal";

const FILE_ATTRIBUTE_OFFLINE: u32 = 0x1000;
const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// What a placeholder's identity blob holds: the entry and the content it shows.
pub(crate) fn blob(entry: EntryId, content: ContentHash) -> Vec<u8> {
    let mut blob = Vec::with_capacity(EntryId::LEN + ContentHash::LEN);
    blob.extend_from_slice(entry.as_bytes());
    blob.extend_from_slice(content.as_bytes());
    blob
}

/// The entry and content a placeholder's identity blob names.
pub(crate) fn read_blob(blob: &[u8]) -> Option<(EntryId, ContentHash)> {
    let (entry, content) = blob.split_at_checked(EntryId::LEN)?;
    let entry: [u8; EntryId::LEN] = entry.try_into().ok()?;
    let content: [u8; ContentHash::LEN] = content.get(..ContentHash::LEN)?.try_into().ok()?;
    Some((EntryId::from_bytes(entry), ContentHash::from_bytes(content)))
}

/// Where the folder path `rel` (`/`-separated) is under `root`.
pub(crate) fn to_abs(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_owned(), |p, part| p.join(part))
}

/// The folder path of `abs` under `root`, `/`-separated, if it is under it. Windows compares
/// paths without regard to case, and may give them in their `\\?\` form.
pub(crate) fn to_rel(root: &Path, abs: &Path) -> Option<String> {
    let parts = |p: &Path| -> Vec<String> {
        p.components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
                // A drive letter in either form, `C:` or `\\?\C:`, is the same drive.
                Component::Prefix(p) => Some(
                    p.as_os_str()
                        .to_string_lossy()
                        .trim_start_matches(r"\\?\")
                        .to_lowercase(),
                ),
                _ => None,
            })
            .collect()
    };
    let (root, abs) = (parts(root), parts(abs));
    let under = abs.len() > root.len()
        && root
            .iter()
            .zip(&abs)
            .all(|(a, b)| a.to_lowercase() == b.to_lowercase());
    under.then(|| abs[root.len()..].join("/"))
}

/// Whether the folder path `rel` is in the reserved directory, or is it.
pub(crate) fn reserved(rel: &str) -> bool {
    rel == RESERVED_DIR || rel.starts_with(&format!("{RESERVED_DIR}/"))
}

/// Whether a file with `meta`, read without following it, has its data elsewhere.
pub(crate) fn dehydrated(meta: &fs::Metadata) -> bool {
    meta.file_attributes()
        & (FILE_ATTRIBUTE_OFFLINE
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
        != 0
}

fn file_time(ms: i64) -> io::Result<nt_time::FileTime> {
    nt_time::FileTime::from_unix_time_nanos(i128::from(ms) * 1_000_000).map_err(io::Error::other)
}

fn placeholder_metadata(file: &FileEntry) -> io::Result<Metadata> {
    let time = file_time(file.modified_ms)?;
    Ok(Metadata::file()
        .size(file.size)
        .written(time)
        .changed(time)
        .created(time))
}

/// Opens `path` as a placeholder to change, alone.
fn open_exclusive(path: &Path) -> io::Result<Placeholder> {
    Placeholder::options()
        .write_access()
        .exclusive()
        .open(path)
        .map_err(io::Error::other)
}

/// Makes the folder `dir` and those above it under `root`, each an in-sync placeholder folder,
/// so another program's rename of one is reported (ADR 0042).
pub(crate) fn make_dirs(root: &Path, dir: &Path) -> io::Result<()> {
    if dir == root || dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        make_dirs(root, parent)?;
    }
    fs::create_dir(dir).or_else(|e| match e.kind() {
        io::ErrorKind::AlreadyExists => Ok(()),
        _ => Err(e),
    })?;
    convert_dir(dir)
}

/// Makes the plain folder `dir` an in-sync placeholder folder.
pub(crate) fn convert_dir(dir: &Path) -> io::Result<()> {
    let mut placeholder = open_exclusive(dir)?;
    if placeholder.info().map_err(io::Error::other)?.is_some() {
        return Ok(());
    }
    placeholder
        .convert_to_placeholder(ConvertOptions::default().mark_in_sync(), None)
        .map(drop)
        .map_err(io::Error::other)
}

/// Makes a dehydrated, in-sync placeholder of `file` under `root`.
pub(crate) fn create(root: &Path, file: &FileEntry) -> io::Result<()> {
    let abs = to_abs(root, &file.path);
    let parent = abs.parent().unwrap_or(root);
    make_dirs(root, parent)?;
    let name = abs
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?;
    PlaceholderCreateInfo::new(name)
        .metadata(placeholder_metadata(file)?)
        .blob(blob(file.entry, file.content))
        .mark_in_sync()
        .create::<&Path>(parent)
        .map(drop)
        .map_err(io::Error::other)
}

/// Opens `path` only to read its attributes and placeholder information: no oplock, sharing
/// everything, so a fill or another program's use of the file is never held up by it.
fn open_to_look(path: &Path) -> Option<Placeholder> {
    const FILE_READ_ATTRIBUTES: u32 = 0x80;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()
        .map(Placeholder::from)
}

/// The entry and content the placeholder at `path` shows, and its pin state, if it is one.
pub(crate) fn identity(path: &Path) -> Option<(Option<(EntryId, ContentHash)>, PinState)> {
    let info = open_to_look(path)?.info().ok()??;
    Some((read_blob(info.blob()), info.pin_state()))
}

/// Points the dehydrated placeholder at `path` at `file`'s current version.
pub(crate) fn update(path: &Path, file: &FileEntry) -> io::Result<()> {
    let blob = blob(file.entry, file.content);
    open_exclusive(path)?
        .update(
            UpdateOptions::default()
                .metadata(placeholder_metadata(file)?)
                .blob(&blob)
                .mark_in_sync()
                .dehydrate(),
            None,
        )
        .map(drop)
        .map_err(io::Error::other)
}

/// Makes the file the engine has at `path`, `file`'s current version, an in-sync placeholder,
/// so it shows as on this device and can be freed. Only while no other program has it open,
/// and only if it is still what the engine recorded, the same file with the same size and
/// modification time: in-sync tells Windows the data is elsewhere too, and it may free it.
/// Returns false, leaving it, otherwise.
pub(crate) fn make_in_sync(path: &Path, file: &FileEntry) -> io::Result<bool> {
    // Shared with no one while it is checked and converted.
    let handle = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
    {
        Ok(handle) => handle,
        Err(e) if e.raw_os_error() == Some(32) => return Ok(false), // ERROR_SHARING_VIOLATION
        Err(e) => return Err(e),
    };
    let meta = handle.metadata()?;
    let modified_ms = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    if meta.len() != file.size || modified_ms != file.modified_ms {
        return Ok(false);
    }
    // Another file moved there, with the same size and time.
    if file.file_id != 0 && winapi_util::file::information(&handle)?.file_index() != file.file_id {
        return Ok(false);
    }
    let blob = blob(file.entry, file.content);
    let mut placeholder = Placeholder::from(handle);
    match placeholder.info().map_err(io::Error::other)? {
        Some(_) => placeholder
            .update(UpdateOptions::default().blob(&blob).mark_in_sync(), None)
            .map(drop),
        None => placeholder
            .convert_to_placeholder(ConvertOptions::default().mark_in_sync().blob(blob), None)
            .map(drop),
    }
    .map_err(io::Error::other)?;
    Ok(true)
}

/// Frees the file at `path` in place, leaving its dehydrated placeholder, if it is the in-sync
/// placeholder of `entry`'s `content` and no other program has it open. Returns whether it did:
/// in sync, Windows has kept it unchanged since it was made so.
pub fn free(path: &Path, entry: EntryId, content: ContentHash) -> io::Result<bool> {
    // Shared with no one while it is checked and freed.
    let handle = match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(path)
    {
        Ok(handle) => handle,
        Err(e) if e.raw_os_error() == Some(32) => return Ok(false), // ERROR_SHARING_VIOLATION
        Err(e) => return Err(e),
    };
    let mut placeholder = Placeholder::from(handle);
    let shows = placeholder
        .info()
        .map_err(io::Error::other)?
        .is_some_and(|info| {
            info.is_in_sync()
                && info.pin_state() != PinState::Pinned
                && read_blob(info.blob()) == Some((entry, content))
        });
    if !shows {
        return Ok(false);
    }
    placeholder
        .update(UpdateOptions::default().dehydrate(), None)
        .map_err(io::Error::other)?;
    Ok(true)
}

/// Whether the placeholder at `path` is in sync.
pub(crate) fn in_sync(path: &Path) -> bool {
    open_to_look(path)
        .and_then(|p| p.info().ok().flatten())
        .is_some_and(|i| i.is_in_sync())
}

/// Fills the placeholder at `path` through the provider, as reading it would, and returns once
/// it is filled or the fill failed. Windows does not always fill a pinned placeholder by
/// itself: Windows Server 2025 and Windows 11 Enterprise never did (ADR 0042). It synchronises
/// this with any fill it started itself, so the content is fetched once either way.
pub(crate) fn hydrate(path: &Path) -> io::Result<()> {
    // Opening it does not fill it; reading its data would. Shared with everyone, so the
    // provider's fill and other programs go on.
    const SHARE_ALL: u32 = 0x7; // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
    let handle = fs::OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        .open(path)?;
    Placeholder::from(handle)
        .hydrate(..)
        .map_err(io::Error::other)
}

/// Sets the pin state of the placeholder at `path`: pinned, or as its folder says.
pub(crate) fn set_pin(path: &Path, pinned: bool) -> io::Result<()> {
    let state = if pinned {
        PinState::Pinned
    } else {
        PinState::Unspecified
    };
    Placeholder::open(path)
        .and_then(|mut p| p.mark_pin(state, PinOptions::default()).map(drop))
        .map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_map_both_ways_whatever_the_case_and_form() {
        let root = Path::new(r"C:\Users\ci\Shoal");
        assert_eq!(
            to_abs(root, "docs/a.txt"),
            Path::new(r"C:\Users\ci\Shoal\docs\a.txt")
        );
        assert_eq!(
            to_rel(root, Path::new(r"C:\Users\ci\Shoal\docs\a.txt")).as_deref(),
            Some("docs/a.txt")
        );
        assert_eq!(
            to_rel(root, Path::new(r"\\?\c:\users\CI\shoal\docs\a.txt")).as_deref(),
            Some("docs/a.txt")
        );
        assert_eq!(to_rel(root, root), None);
        assert_eq!(to_rel(root, Path::new(r"C:\Users\ci\Other\a.txt")), None);
    }

    #[test]
    fn a_blob_names_its_entry_and_content() {
        let (entry, content) = (
            EntryId::from_bytes([1; 16]),
            ContentHash::from_bytes([2; 32]),
        );
        assert_eq!(read_blob(&blob(entry, content)), Some((entry, content)));
        assert_eq!(read_blob(b"short"), None);
    }

    #[test]
    fn the_reserved_directory_is_its_own() {
        assert!(reserved(".shoal"));
        assert!(reserved(".shoal/history/x"));
        assert!(!reserved(".shoalish"));
        assert!(!reserved("docs/.shoal"));
    }
}
