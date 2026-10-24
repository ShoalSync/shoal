//! blobs/1 frames.

#![no_main]

use libfuzzer_sys::fuzz_target;
use shoal_proto::blobs::BlobsMessage;
use shoal_proto::frame;

fuzz_target!(|data: &[u8]| {
    let _ = frame::decode::<BlobsMessage>(data);
});
