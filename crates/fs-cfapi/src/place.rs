//! Where a sync root can be (ADR 0042).

use std::fs;
use std::os::windows::fs::MetadataExt;
use std::path::{Component, Path, Prefix};

use cloud_filter::root::active_roots;
use shoal_proto::FolderId;

const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

/// Why `root` cannot be `folder`'s sync root, if it cannot: it must be a directory, not on a
/// network share, inside no hidden or system folder (Explorer would not show it), and neither
/// inside nor around another sync root, another provider's or another folder's.
pub fn unusable(root: &Path, folder: FolderId) -> Option<String> {
    match fs::metadata(root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Some(format!("{} is not a directory", root.display())),
        Err(e) => return Some(format!("cannot read {}: {e}", root.display())),
    }
    // Another volume Windows cannot keep placeholders on, as FAT or a mapped drive, fails
    // registration, with Windows' reason.
    if matches!(
        root.components().next(),
        Some(Component::Prefix(p)) if matches!(p.kind(), Prefix::UNC(..) | Prefix::VerbatimUNC(..))
    ) {
        return Some(format!("{} is on a network share", root.display()));
    }
    // The volume's own root reads as hidden and system, and is no folder of the user's.
    let mut dir = Some(root);
    while let Some(here) = dir {
        let parent = here.parent();
        if parent.is_some() {
            let attributes = fs::metadata(here).map_or(0, |m| m.file_attributes());
            if attributes & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0 {
                return Some(format!(
                    "{} is hidden, and Explorer does not show a cloud folder inside it",
                    here.display()
                ));
            }
        }
        dir = parent;
    }
    let ours = crate::sync_root_id(folder).ok()?.to_os_string();
    for other in active_roots().ok()? {
        if other.id().to_os_string() == ours {
            continue;
        }
        let path = other.path();
        if within(root, &path) || within(&path, root) {
            return Some(format!(
                "it overlaps {}'s cloud folder at {}",
                other.display_name().to_string_lossy(),
                path.display()
            ));
        }
    }
    None
}

/// Whether `inner` is `outer` or inside it, however either is spelled.
pub(crate) fn within(inner: &Path, outer: &Path) -> bool {
    let (inner, outer) = (parts(inner), parts(outer));
    !outer.is_empty() && inner.starts_with(&outer)
}

/// A path's components, lowercased, with a verbatim prefix spelled as the plain one.
fn parts(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|c| match c {
            Component::Prefix(prefix) => Some(match prefix.kind() {
                Prefix::VerbatimDisk(d) | Prefix::Disk(d) => format!("{}:", d as char),
                Prefix::VerbatimUNC(server, share) | Prefix::UNC(server, share) => format!(
                    r"\\{}\{}",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                ),
                _ => prefix.as_os_str().to_string_lossy().into_owned(),
            }),
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .map(|p| p.to_lowercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn within_ignores_case_and_spelling() {
        let outer = Path::new(r"C:\Users\ci\OneDrive");
        assert!(within(Path::new(r"c:\users\CI\onedrive\Docs"), outer));
        assert!(within(Path::new(r"\\?\C:\Users\ci\OneDrive"), outer));
        assert!(!within(Path::new(r"C:\Users\ci\OneDriveX"), outer));
        assert!(!within(Path::new(r"C:\Users\ci"), outer));
    }

    #[test]
    fn a_folder_in_the_profile_can_be_one() {
        let profile = std::env::var_os("USERPROFILE").expect("USERPROFILE");
        let root = tempfile::Builder::new()
            .prefix("Shoal place ")
            .tempdir_in(profile)
            .unwrap();
        assert_eq!(unusable(root.path(), FolderId([7; 32])), None);
        let hidden = root.path().join("hidden");
        fs::create_dir(&hidden).unwrap();
        let status = std::process::Command::new("attrib")
            .arg("+H")
            .arg(&hidden)
            .status()
            .unwrap();
        assert!(status.success());
        let inside = hidden.join("Shoal");
        fs::create_dir(&inside).unwrap();
        let problem = unusable(&inside, FolderId([7; 32])).unwrap();
        assert!(problem.contains("hidden"), "{problem}");
    }
}
