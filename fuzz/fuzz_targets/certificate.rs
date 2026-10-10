//! A device certificate as a peer sends it.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::Signed;

fuzz_target!(|data: &[u8]| {
    if let Ok(signed) = postcard::from_bytes::<Signed<DeviceCertificate>>(data) {
        let _ = signed.verify_certificate();
    }
});
