//! sync/1 frames.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tidefell_proto::frame;
use tidefell_proto::sync::SyncMessage;

fuzz_target!(|data: &[u8]| {
    let _ = frame::decode::<SyncMessage>(data);
});
