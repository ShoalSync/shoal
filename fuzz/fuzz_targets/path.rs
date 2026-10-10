//! Path validation: a path a peer's version carries is either refused or stays inside the folder.

#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::{Component, Path};

use tidefell_proto::sync::is_valid_path;

fuzz_target!(|data: &[u8]| {
    let Ok(path) = std::str::from_utf8(data) else {
        return;
    };
    if is_valid_path(path) {
        assert!(!path.contains('\\'), "{path:?}");
        for component in Path::new(path).components() {
            assert!(
                matches!(component, Component::Normal(_)),
                "{path:?} leaves the folder"
            );
        }
    }
});
