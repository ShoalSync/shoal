//! The `tidefell/pair/2` protocol and pairing codes. See `specs/pair-v2.md` and ADR 0015.
//!
//! Variant order is part of the wire format. Append new variants only in a new protocol version.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::identity::DeviceCertificate;
use crate::{DeviceId, Error, FolderId, Signed, UserId};

/// Prefix of a pairing code's text form.
pub const CODE_PREFIX: &str = "TFP1-";

/// What a new device shows so a device holding the user key can pair with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingCode {
    /// The new device.
    pub device: DeviceId,
    /// One-time secret; proves the approver has seen the code.
    pub secret: [u8; 16],
    /// Relay the new device is reachable through, if any.
    pub relay: Option<String>,
    /// Direct addresses of the new device.
    pub addrs: Vec<SocketAddr>,
}

impl PairingCode {
    /// The text form: [`CODE_PREFIX`] followed by the postcard encoding in unpadded RFC 4648
    /// base32 (upper case, which QR codes store compactly).
    #[expect(
        clippy::expect_used,
        reason = "postcard fails to encode only sequences of unknown length, which no message has"
    )]
    pub fn to_text(&self) -> String {
        let bytes = postcard::to_stdvec(self).expect("in-memory encoding cannot fail");
        format!("{CODE_PREFIX}{}", base32_encode(&bytes))
    }

    /// Parses the text form. Case, surrounding whitespace and embedded spaces or dashes after
    /// the prefix are ignored, so codes survive being read out or retyped.
    pub fn from_text(text: &str) -> Result<Self, Error> {
        let text = text.trim();
        let body = text
            .get(..CODE_PREFIX.len())
            .filter(|p| p.eq_ignore_ascii_case(CODE_PREFIX))
            .map(|_| &text[CODE_PREFIX.len()..])
            .ok_or(Error::InvalidCode)?;
        let bytes = base32_decode(body).ok_or(Error::InvalidCode)?;
        postcard::from_bytes(&bytes).map_err(|_| Error::InvalidCode)
    }
}

/// Why a pairing was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairErrorCode {
    /// The approver's proof does not match the code's secret.
    BadProof,
    /// The code expired or was already used.
    Expired,
    /// The person declined on the approver.
    Declined,
    /// The certificate was not for this device or did not verify.
    BadCertificate,
    /// The message was malformed or unexpected.
    ProtocolViolation,
}

/// A folder the new device may join after pairing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderOffer {
    /// Folder ID.
    pub id: FolderId,
    /// A display name: the last component of the folder's path on the approver.
    pub name: String,
    /// Users allowed to sync the folder.
    pub members: Vec<UserId>,
    /// Recycle-bin retention in days; 0 keeps deleted files forever.
    pub retention_days: u32,
    /// For an encrypted folder, what the new device joins it with (ADR 0023).
    pub encryption: Option<OfferedEncryption>,
}

/// How an offered folder is encrypted: the new device joins it as encrypted, with these storage
/// peers, and gets its keys by grant once it has joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferedEncryption {
    /// The approver's storage peers for the folder: devices that hold it without reading it.
    pub storage_peers: Vec<DeviceId>,
}

/// A message on a `tidefell/pair/2` stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PairMessage {
    /// Approver → new device, first: proof that the approver has seen the code.
    Request {
        /// The approver's own device certificate, naming its user.
        approver: Signed<DeviceCertificate>,
        /// Keyed BLAKE3 of both device IDs under the code's secret (see the spec).
        proof: [u8; 32],
    },
    /// New device → approver: the proof is valid.
    Accept {
        /// The name the new device suggests for itself, e.g. its model.
        name: String,
    },
    /// Approver → new device: the person confirmed; here is the new device's certificate.
    Complete {
        /// Certificate for the new device, signed by the approver's user.
        certificate: Signed<DeviceCertificate>,
        /// The approver's relay, if any.
        relay: Option<String>,
        /// The approver's direct addresses.
        addrs: Vec<SocketAddr>,
        /// Folders of the user on the approver.
        folders: Vec<FolderOffer>,
    },
    /// Either side: pairing refused; the stream closes.
    Reject {
        /// Reason.
        code: PairErrorCode,
    },
}

const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

#[expect(
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    reason = "bits stays below 13, and an index masked with 31 is within the 32-letter alphabet"
)]
fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "bits stays below 13, and a length times 5 fits when the text is in memory"
)]
fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in text.chars().filter(|c| !matches!(c, ' ' | '-')) {
        let value = ALPHABET
            .iter()
            .position(|&a| a == c.to_ascii_uppercase() as u8)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    // Leftover bits are padding and must be zero.
    (buffer & ((1 << bits) - 1) == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> PairingCode {
        PairingCode {
            device: DeviceId([0xab; 32]),
            secret: [0x5a; 16],
            relay: Some("https://relay.tidefell.org./".into()),
            addrs: vec![
                "192.168.1.20:7777".parse().unwrap(),
                "[::1]:7778".parse().unwrap(),
            ],
        }
    }

    #[test]
    fn base32_matches_rfc_4648() {
        for (input, expected) in [
            ("", ""),
            ("f", "MY"),
            ("fo", "MZXQ"),
            ("foo", "MZXW6"),
            ("foob", "MZXW6YQ"),
            ("fooba", "MZXW6YTB"),
            ("foobar", "MZXW6YTBOI"),
        ] {
            assert_eq!(base32_encode(input.as_bytes()), expected);
            assert_eq!(base32_decode(expected).unwrap(), input.as_bytes());
        }
        assert!(
            base32_decode("MZXW6YQ1").is_none(),
            "1 is not in the alphabet"
        );
        assert!(base32_decode("MZ").is_none(), "non-zero padding bits");
    }

    #[test]
    fn code_round_trips_through_text() {
        let text = code().to_text();
        assert!(text.starts_with(CODE_PREFIX));
        assert!(text[CODE_PREFIX.len()..]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
        assert_eq!(PairingCode::from_text(&text).unwrap(), code());
        let retyped = format!("  {}  ", text.to_lowercase());
        assert_eq!(PairingCode::from_text(&retyped).unwrap(), code());
        assert!(PairingCode::from_text("TFP1-").is_err());
        assert!(PairingCode::from_text("hello").is_err());
    }
}
