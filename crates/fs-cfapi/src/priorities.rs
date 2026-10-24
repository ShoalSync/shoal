//! An encrypted folder's storage priorities in Explorer (ADR 0023's section of 2026-10-23): a
//! state of the provider's own on each file and directory not at the default, its icon in the
//! Status column and its words in the icon's tooltip. Read only: they are set in the tray's
//! Storage window, or in `.tidefellpriorities`.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use cloud_filter::properties::{set_item_properties, ItemProperty};

/// The id of the state in the sync root's registration.
pub(crate) const PROPERTY: i32 = 1;

/// Its name in the registration.
pub(crate) const NAME: &str = "Storage priority";

/// A priority no rule sets: nothing shown.
const DEFAULT: u8 = 128;

/// What Explorer shows for `priority`: none at the default.
fn shown(priority: u8) -> Option<ItemProperty> {
    let (word, icon) = match priority {
        DEFAULT => return None,
        0 => (
            "never stored on storage peers",
            r"%SystemRoot%\system32\shell32.dll,131",
        ),
        1..=127 => (
            "kept after others",
            r"%SystemRoot%\system32\shell32.dll,247",
        ),
        _ => ("kept first", r"%SystemRoot%\system32\shell32.dll,43"),
    };
    Some(ItemProperty {
        id: PROPERTY,
        value: format!("Storage priority {priority}: {word}"),
        icon: icon.to_owned(),
    })
}

/// Shows `priorities`, each folder-relative path's, on the files and directories under `root`
/// that are there, changing only those whose priority changed since `last`, which it updates.
/// A path that is not there, or that Windows refuses, is left for the next time. Blocks.
pub(crate) fn show(
    root: &Path,
    priorities: &[(String, u8)],
    last: &mut HashMap<String, u8>,
) -> io::Result<usize> {
    let wanted: HashMap<&str, u8> = priorities
        .iter()
        .filter(|(_, p)| *p != DEFAULT)
        .map(|(path, p)| (path.as_str(), *p))
        .collect();
    let mut changes: Vec<(String, u8)> = wanted
        .iter()
        .filter(|(path, p)| last.get(**path) != Some(*p))
        .map(|(path, p)| ((*path).to_owned(), *p))
        .collect();
    // Back to the default: cleared.
    changes.extend(
        last.keys()
            .filter(|path| !wanted.contains_key(path.as_str()))
            .map(|path| (path.clone(), DEFAULT)),
    );
    let mut done = 0;
    for (path, priority) in changes {
        let abs = root.join(&path);
        if !abs.exists() {
            last.remove(&path);
            continue;
        }
        let properties: Vec<ItemProperty> = shown(priority).into_iter().collect();
        match set_item_properties(&abs, &properties) {
            Ok(()) => {
                if priority == DEFAULT {
                    last.remove(&path);
                } else {
                    last.insert(path, priority);
                }
                done += 1;
            }
            Err(e) => tracing::debug!(path, error = %e, "cannot show the storage priority"),
        }
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_priorities_not_at_the_default_show() {
        assert!(shown(128).is_none());
        assert_eq!(
            shown(0).unwrap().value,
            "Storage priority 0: never stored on storage peers"
        );
        assert!(shown(50).unwrap().icon.ends_with(",247"));
        assert!(shown(200).unwrap().icon.ends_with(",43"));
    }
}
