//! A folder metadata snapshot read from disk, which may be truncated or corrupted.

#![no_main]

use libfuzzer_sys::fuzz_target;
use shoal_meta::Folder;

fuzz_target!(|data: &[u8]| {
    if let Ok(folder) = Folder::from_snapshot(data) {
        let _ = Folder::from_snapshot(&folder.to_snapshot());
    }
});
