//! Encryption keys and key grants: how a folder key reaches a device (`specs/encryption-v1.md`).

use std::fmt;

use tidefell_proto::{DeviceId, FolderId, KeyId};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::{hpke, random, Error, FolderKey, Result};

/// A device's X25519 key for receiving folder keys, separate from its identity key. It is kept
/// with the identity key, not in a folder's state, so a rebuilt folder can be granted again.
#[derive(Clone)]
pub struct EncryptionSecret(StaticSecret);

impl fmt::Debug for EncryptionSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EncryptionSecret({:02x?})", self.public())
    }
}

impl EncryptionSecret {
    /// A new random key.
    pub fn generate() -> Result<Self> {
        Ok(Self::from_bytes(random()?))
    }

    /// The key from its stored bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(StaticSecret::from(bytes))
    }

    /// The bytes to store.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// The key HPKE derives from `ikm`: for golden vectors.
    pub fn from_ikm(ikm: &[u8]) -> Self {
        Self(hpke::derive_key_pair(ikm))
    }

    /// The public key a device publishes, signed, as its `EncryptionKey`.
    pub fn public(&self) -> [u8; 32] {
        PublicKey::from(&self.0).to_bytes()
    }
}

/// Whom a grant is for: a device of a folder, by the encryption key it published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantFor {
    pub folder: FolderId,
    pub recipient: DeviceId,
    pub recipient_key: [u8; 32],
}

impl GrantFor {
    /// HPKE's `info`: binds the grant to its folder, key and recipient.
    fn info(&self, generation: u64, key: &KeyId) -> Vec<u8> {
        [
            b"tidefell/key-grant/1".as_slice(),
            self.folder.as_bytes(),
            &generation.to_be_bytes(),
            key.as_bytes(),
            self.recipient.as_bytes(),
            &self.recipient_key,
        ]
        .concat()
    }
}

/// Seals `key` to `to`: HPKE's encapsulated key and the sealed key.
pub fn seal_grant(key: &FolderKey, to: &GrantFor) -> Result<([u8; 32], Vec<u8>)> {
    seal_grant_with(key, to, &random::<32>()?)
}

/// [`seal_grant`] with the ephemeral key derived from `ikm`: for golden vectors only.
pub fn seal_grant_with(key: &FolderKey, to: &GrantFor, ikm: &[u8]) -> Result<([u8; 32], Vec<u8>)> {
    let info = to.info(key.generation(), &key.id());
    hpke::seal(&to.recipient_key, &info, b"", key.as_bytes(), ikm)
}

/// Opens a grant of key `key_id`, generation `generation`, made for `to` and sealed to
/// `secret`, and checks the key is the one it names.
pub fn open_grant(
    secret: &EncryptionSecret,
    to: &GrantFor,
    generation: u64,
    key_id: &KeyId,
    enc: &[u8; 32],
    sealed: &[u8],
) -> Result<FolderKey> {
    if to.recipient_key != secret.public() {
        return Err(Error::Mismatch("encryption key"));
    }
    let opened = hpke::open(&secret.0, enc, &to.info(generation, key_id), b"", sealed)?;
    let bytes: [u8; 32] = opened
        .try_into()
        .map_err(|_| Error::Malformed("granted key"))?;
    let key = FolderKey::new(bytes, generation);
    if key.id() != *key_id {
        return Err(Error::Mismatch("granted key"));
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_open_only_for_their_recipient_and_key() {
        let key = FolderKey::new([9; 32], 3);
        let secret = EncryptionSecret::generate().unwrap();
        let to = GrantFor {
            folder: FolderId([1; 32]),
            recipient: DeviceId([2; 32]),
            recipient_key: secret.public(),
        };
        let (enc, sealed) = seal_grant(&key, &to).unwrap();
        assert_eq!(sealed.len(), 48);
        let opened = open_grant(&secret, &to, 3, &key.id(), &enc, &sealed).unwrap();
        assert_eq!(opened.as_bytes(), key.as_bytes());
        assert_eq!(opened.generation(), 3);

        // Another device's secret, another generation, folder or recipient: it does not open.
        let other = EncryptionSecret::generate().unwrap();
        assert_eq!(
            open_grant(&other, &to, 3, &key.id(), &enc, &sealed).unwrap_err(),
            Error::Mismatch("encryption key")
        );
        assert_eq!(
            open_grant(&secret, &to, 4, &key.id(), &enc, &sealed).unwrap_err(),
            Error::Open
        );
        let elsewhere = GrantFor {
            folder: FolderId([5; 32]),
            ..to
        };
        assert_eq!(
            open_grant(&secret, &elsewhere, 3, &key.id(), &enc, &sealed).unwrap_err(),
            Error::Open
        );
        let stored = EncryptionSecret::from_bytes(secret.to_bytes());
        assert_eq!(stored.public(), secret.public());
    }
}
