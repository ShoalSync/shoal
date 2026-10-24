//! Device certificate files, so a certificate issued where the user key lives can be copied to
//! the device.
//!
//! Format (version 1):
//!
//! ```text
//! shoal-device-certificate 1
//! <lowercase hex of the postcard-encoded Signed<DeviceCertificate>>
//! ```

use std::fs;
use std::path::Path;

use shoal_proto::identity::DeviceCertificate;
use shoal_proto::Signed;

use crate::{Error, Result};

const HEADER: &str = "shoal-device-certificate 1";

/// Encodes a certificate in the file format.
pub fn encode_certificate(certificate: &Signed<DeviceCertificate>) -> String {
    let bytes = postcard::to_stdvec(certificate).expect("in-memory encoding cannot fail");
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{HEADER}\n{hex}\n")
}

/// Decodes and verifies a certificate in the file format.
pub fn decode_certificate(text: &str) -> Result<Signed<DeviceCertificate>> {
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) {
        return Err(Error::MalformedKey("unknown certificate header"));
    }
    let hex = lines
        .next()
        .ok_or(Error::MalformedKey("missing certificate"))?
        .trim();
    if hex.len() % 2 != 0 {
        return Err(Error::MalformedKey("odd-length hex"));
    }
    let bytes = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .ok_or(Error::MalformedKey("not hex"))
        })
        .collect::<Result<Vec<u8>>>()?;
    let certificate: Signed<DeviceCertificate> =
        postcard::from_bytes(&bytes).map_err(|_| Error::MalformedKey("not a certificate"))?;
    certificate
        .verify_certificate()
        .map_err(|_| Error::MalformedKey("certificate signature is invalid"))?;
    Ok(certificate)
}

/// Writes a certificate file, replacing any existing one.
pub fn save_certificate(path: &Path, certificate: &Signed<DeviceCertificate>) -> Result<()> {
    fs::write(path, encode_certificate(certificate))?;
    Ok(())
}

/// Reads and verifies a certificate file.
pub fn load_certificate(path: &Path) -> Result<Signed<DeviceCertificate>> {
    decode_certificate(&fs::read_to_string(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeviceKey, UserKey};

    #[test]
    fn round_trip_and_tamper() {
        let user = UserKey::from_seed([1; 32]);
        let device = DeviceKey::from_seed([2; 32]);
        let cert = user.certify(device.id(), "laptop", 5);
        let text = encode_certificate(&cert);
        assert_eq!(decode_certificate(&text).unwrap(), cert);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.cert");
        save_certificate(&path, &cert).unwrap();
        assert_eq!(load_certificate(&path).unwrap(), cert);

        // Flip one hex digit in the signature (the end of the encoding).
        let mut tampered = text.trim_end().to_string();
        let last = tampered.pop().unwrap();
        tampered.push(if last == '0' { '1' } else { '0' });
        assert!(decode_certificate(&tampered).is_err());
    }
}
