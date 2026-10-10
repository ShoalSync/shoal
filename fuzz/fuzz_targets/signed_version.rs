//! A signed version as a peer sends it: decoded, verified and checked against its author's
//! certificate.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tidefell_proto::sync::EntryVersion;
use tidefell_proto::{Signed, SignedKind};

fuzz_target!(|data: &[u8]| {
    let _ = EntryVersion::decode(data);
    if let Ok(signed) = postcard::from_bytes::<Signed<EntryVersion>>(data) {
        let _ = signed.verify();
        let _ = signed.verify_authored();
    }
});
