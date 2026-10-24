//! sync/1 frames.

#![no_main]

use libfuzzer_sys::fuzz_target;
use shoal_proto::frame;
use shoal_proto::sync::SyncMessage;

fuzz_target!(|data: &[u8]| {
    let _ = frame::decode::<SyncMessage>(data);
});
