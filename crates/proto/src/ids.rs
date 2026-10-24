//! Fixed-size identifiers. All are displayed and parsed as lowercase hex.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Error;

macro_rules! id {
    ($(#[$doc:meta])* $name:ident, $len:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(#[serde(with = "crate::bytes")] pub [u8; $len]);

        impl $name {
            /// Length in bytes.
            pub const LEN: usize = $len;

            /// Wraps raw bytes.
            pub const fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            /// Returns the raw bytes.
            pub const fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in &self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({self})", stringify!($name))
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(s: &str) -> Result<Self, Error> {
                let s = s.as_bytes();
                if s.len() != $len * 2 {
                    return Err(Error::InvalidHex);
                }
                let mut out = [0u8; $len];
                for (byte, pair) in out.iter_mut().zip(s.chunks_exact(2)) {
                    let &[high, low] = pair else {
                        return Err(Error::InvalidHex);
                    };
                    *byte = (hex_val(high)? << 4) | hex_val(low)?;
                }
                Ok(Self(out))
            }
        }
    };
}

impl From<DeviceId> for PublicKey {
    fn from(id: DeviceId) -> Self {
        Self(id.0)
    }
}

impl From<UserId> for PublicKey {
    fn from(id: UserId) -> Self {
        Self(id.0)
    }
}

#[expect(
    clippy::arithmetic_side_effects,
    reason = "each subtraction is within the range its arm matched"
)]
fn hex_val(c: u8) -> Result<u8, Error> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(Error::InvalidHex),
    }
}

id!(
    /// An Ed25519 public key of any signer: a device or a user.
    PublicKey, 32
);
id!(
    /// A device's Ed25519 public key. Also the device's iroh endpoint ID.
    DeviceId, 32
);
id!(
    /// A user's Ed25519 public key.
    UserId, 32
);
id!(
    /// A folder's random identifier, chosen when the folder is created.
    FolderId, 32
);
id!(
    /// A stable identifier for one entry (path) in a folder, chosen when the entry is created.
    EntryId, 16
);
id!(
    /// BLAKE3 hash of one chunk's bytes.
    ChunkHash, 32
);
id!(
    /// BLAKE3 hash of a file's full content.
    ContentHash, 32
);
id!(
    /// Names an encrypted folder's key without revealing it: derived from the key
    /// (`specs/encryption-v1.md`).
    KeyId, 16
);
id!(
    /// An encrypted folder's chunk: the BLAKE3 hash of its plaintext keyed by the folder key, so
    /// only members can compute it (`specs/encryption-v1.md`).
    ChunkId, 32
);
id!(
    /// BLAKE3 hash of a list blob, a plaintext list of an encrypted file's chunk IDs
    /// (`specs/encryption-v1.md`).
    ListId, 32
);
id!(
    /// What `shoal/blobs/2` transfers: a sealed chunk's chunk ID or a list blob's list ID.
    /// Which it is, the requester knows.
    BlobId, 32
);

impl From<ChunkId> for BlobId {
    fn from(id: ChunkId) -> Self {
        Self(id.0)
    }
}

impl From<ListId> for BlobId {
    fn from(id: ListId) -> Self {
        Self(id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trip() {
        let id = EntryId([0xab; 16]);
        let s = id.to_string();
        assert_eq!(s, "ab".repeat(16));
        assert_eq!(s.parse::<EntryId>().unwrap(), id);
    }

    #[test]
    fn rejects_bad_hex() {
        assert!("AB".repeat(16).parse::<EntryId>().is_err());
        assert!("ab".parse::<EntryId>().is_err());
    }

    #[test]
    fn encodes_as_raw_bytes() {
        let bytes = postcard::to_stdvec(&ChunkHash([7; 32])).unwrap();
        assert_eq!(bytes, vec![7; 32]);
    }
}
