//! What a version was received as, or made into: a signed `EntryVersion` in a plaintext folder,
//! or a signed `SealedVersion` in an encrypted one (ADR 0023, "On member devices"). The folder
//! itself always works on the decrypted [`EntryVersion`]; this form is what the database keeps,
//! what is served, and what a conflict copy's ID hashes.

use ed25519_dalek::SigningKey;
use tidefell_proto::sync::EntryVersion;
use tidefell_proto::sync2::SealedVersion;
use tidefell_proto::{DeviceId, Signed, SignedKind};

use crate::{Error, Result};

/// A version as signed by its author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wire {
    /// A plaintext folder's version (sync/1).
    Plain(Signed<EntryVersion>),
    /// An encrypted folder's version (sync/2): what members read is sealed inside.
    Sealed(Signed<SealedVersion>),
}

impl Wire {
    /// The signed body: what conflict copy IDs hash.
    pub fn body(&self) -> &[u8] {
        match self {
            Wire::Plain(signed) => &signed.body,
            Wire::Sealed(signed) => &signed.body,
        }
    }

    /// The plaintext folder's signed version, if this is one.
    pub fn plain(&self) -> Option<&Signed<EntryVersion>> {
        match self {
            Wire::Plain(signed) => Some(signed),
            Wire::Sealed(_) => None,
        }
    }

    /// The encrypted folder's signed version, if this is one.
    pub fn sealed(&self) -> Option<&Signed<SealedVersion>> {
        match self {
            Wire::Sealed(signed) => Some(signed),
            Wire::Plain(_) => None,
        }
    }

    /// Checks that `version` is what this carries: decoded from it, for a plaintext version, or
    /// matching its clear fields, for a sealed one whose payload was opened elsewhere. Signatures
    /// are not checked again: they were when the version arrived.
    pub(crate) fn check(&self, version: &EntryVersion) -> Result<()> {
        let (author, signer) = match self {
            Wire::Plain(signed) => {
                let decoded = EntryVersion::decode(&signed.body).map_err(Error::Snapshot)?;
                if decoded != *version {
                    return Err(Error::Snapshot(postcard::Error::DeserializeBadEncoding));
                }
                (decoded.author, signed.signer)
            }
            Wire::Sealed(signed) => {
                let sealed = SealedVersion::decode(&signed.body).map_err(Error::Snapshot)?;
                if sealed.folder != version.folder
                    || sealed.entry != version.entry
                    || sealed.author != version.author
                    || sealed.version != version.version
                {
                    return Err(Error::Snapshot(postcard::Error::DeserializeBadEncoding));
                }
                (sealed.author, signed.signer)
            }
        };
        if tidefell_proto::PublicKey::from(author) != signer {
            return Err(tidefell_proto::Error::SignerMismatch.into());
        }
        Ok(())
    }
}

impl From<Signed<EntryVersion>> for Wire {
    fn from(signed: Signed<EntryVersion>) -> Self {
        Wire::Plain(signed)
    }
}

/// Makes a local change into what is sent: signs it, or for an encrypted folder seals and signs
/// it. Implemented for a device's [`SigningKey`], which signs plaintext versions.
pub trait Seal {
    /// The device that signs.
    fn device(&self) -> DeviceId;
    /// The version as sent.
    fn seal(&self, version: &EntryVersion) -> Result<Wire>;
}

impl Seal for SigningKey {
    fn device(&self) -> DeviceId {
        DeviceId(self.verifying_key().to_bytes())
    }

    fn seal(&self, version: &EntryVersion) -> Result<Wire> {
        Ok(Wire::Plain(Signed::sign(version, self)))
    }
}
