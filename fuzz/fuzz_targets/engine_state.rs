//! A folder's engine state and metadata files read from disk, which may be truncated or
//! corrupted.

#![no_main]

use libfuzzer_sys::fuzz_target;
use shoal_engine::fuzzing::open_folder;

fuzz_target!(|data: &[u8]| {
    // The first byte says where the metadata ends and the state begins.
    let Some((&split, rest)) = data.split_first() else {
        return;
    };
    let (meta, state) = rest.split_at(usize::from(split).min(rest.len()));
    open_folder(meta, state);
});
