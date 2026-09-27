//! Pins on partial replicas (ADR 0029): files and directories this device keeps on disk,
//! fetched in the background, never evicted and not counted against the budget.

use std::collections::BTreeSet;

/// Whether the folder-relative `path` is pinned: it, or a directory it is in, is in `pins`.
pub(crate) fn is_pinned(pins: &BTreeSet<String>, path: &str) -> bool {
    if pins.is_empty() {
        return false;
    }
    let mut at = path;
    loop {
        if pins.contains(at) {
            return true;
        }
        match at.rsplit_once('/') {
            Some((parent, _)) => at = parent,
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pin_covers_its_path_and_what_is_below() {
        let pins: BTreeSet<String> = ["photos/2024".into(), "notes.txt".into()].into();
        assert!(is_pinned(&pins, "photos/2024"));
        assert!(is_pinned(&pins, "photos/2024/beach.jpg"));
        assert!(is_pinned(&pins, "photos/2024/06/sea.jpg"));
        assert!(is_pinned(&pins, "notes.txt"));
        assert!(!is_pinned(&pins, "photos/2024b/x.jpg"));
        assert!(!is_pinned(&pins, "photos/2023/x.jpg"));
        assert!(!is_pinned(&pins, "photos"));
        assert!(!is_pinned(&pins, "notes.txt.bak"));
        assert!(!is_pinned(&BTreeSet::new(), "anything"));
    }
}
