//! pair/2 frames.

#![no_main]

use libfuzzer_sys::fuzz_target;
use shoal_proto::frame;
use shoal_proto::pair::PairMessage;

fuzz_target!(|data: &[u8]| {
    let _ = frame::decode::<PairMessage>(data);
});
