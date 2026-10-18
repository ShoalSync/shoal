//! Encrypted folders' keys, sealing and key grants (ADR 0023; `specs/encryption-v1.md`).
//!
//! A folder key seals what members write so that storage peers hold and serve it without reading
//! it: chunks under keyed chunk IDs ([`FolderKey::seal_chunk`]), list blobs of chunk IDs in the
//! clear ([`list_blob`]), and each version's path, kind, time and chunk lengths
//! ([`FolderKey::seal_payload`]). Folder keys reach devices in grants, sealed to each device's
//! X25519 encryption key with HPKE ([`seal_grant`], [`open_grant`]).

mod grant;
mod hpke;
mod key;
mod list;

pub use grant::{open_grant, seal_grant, seal_grant_with, EncryptionSecret, GrantFor};
pub use key::{FolderKey, MAX_CHUNK};
pub use list::{list_blob, read_list_blob, MAX_LIST};

/// Why sealed data could not be opened or made.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The authentication tag does not match: wrong key, or changed in transit or at rest.
    #[error("sealed data does not open: wrong key or changed")]
    Open,
    /// Opened, but the padding is not as the spec writes it.
    #[error("sealed data has malformed padding")]
    Padding,
    /// Opened, but the plaintext is not what its name says: a chunk whose keyed hash is not its
    /// ID, a list blob whose hash is not its ID, or a granted key whose key ID differs.
    #[error("{0} does not match its ID")]
    Mismatch(&'static str),
    /// Larger than the spec allows, or a list too long.
    #[error("{0} is too large")]
    TooLarge(&'static str),
    /// Not encodable or decodable as the spec says.
    #[error("malformed {0}")]
    Malformed(&'static str),
    /// The operating system's random number generator failed.
    #[error("no random bytes: {0}")]
    Random(String),
}

/// Result of this crate's operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Random bytes from the operating system.
fn random<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| Error::Random(e.to_string()))?;
    Ok(bytes)
}

/// `data || 0x01 || 0x00 * n`, the least `n` making the length a multiple of `block`.
fn pad(data: &[u8], block: usize) -> Vec<u8> {
    let mut padded = Vec::with_capacity(data.len().saturating_add(block));
    padded.extend_from_slice(data);
    padded.push(1);
    while padded.len() % block != 0 {
        padded.push(0);
    }
    padded
}

/// The data [`pad`] padded: at most `block - 1` trailing zero bytes, and the 0x01 before them.
fn unpad(mut padded: Vec<u8>, block: usize) -> Result<Vec<u8>> {
    let zeros = padded.iter().rev().take_while(|b| **b == 0).count();
    if zeros >= block {
        return Err(Error::Padding);
    }
    padded.truncate(padded.len().saturating_sub(zeros));
    if padded.pop() != Some(1) {
        return Err(Error::Padding);
    }
    Ok(padded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_round_trips_and_refuses_what_it_did_not_make() {
        for len in [0, 1, 62, 63, 64, 65, 4095, 4096, 5000] {
            let data: Vec<u8> = (0..len).map(|n| (n % 251) as u8 | 1).collect();
            for block in [64, 4096] {
                let padded = pad(&data, block);
                assert_eq!(padded.len() % block, 0);
                assert_eq!(unpad(padded, block).unwrap(), data);
            }
        }
        // Data ending in zero bytes keeps them: the terminator marks where it ends.
        assert_eq!(unpad(pad(&[5, 0, 0], 64), 64).unwrap(), vec![5, 0, 0]);
        assert_eq!(unpad(vec![0; 64], 64), Err(Error::Padding));
        assert_eq!(unpad(vec![7; 64], 64), Err(Error::Padding));
        let mut long = vec![1];
        long.extend([0; 64]);
        assert_eq!(unpad(long, 64), Err(Error::Padding));
    }
}
