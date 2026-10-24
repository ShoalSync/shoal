//! Pairing codes, as typed or scanned.

#![no_main]

use libfuzzer_sys::fuzz_target;
use shoal_proto::pair::PairingCode;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(code) = PairingCode::from_text(text) {
            assert_eq!(PairingCode::from_text(&code.to_text()).ok(), Some(code));
        }
    }
});
