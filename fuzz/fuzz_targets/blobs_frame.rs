//! blobs/1 frames.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tidefell_proto::blobs::BlobsMessage;
use tidefell_proto::frame;

fuzz_target!(|data: &[u8]| {
    let _ = frame::decode::<BlobsMessage>(data);
});
