//! Identity messages: device certificates and their revocations.
//!
//! A user proves that a device belongs to them by signing a [`DeviceCertificate`] with their
//! user key. Peers accept a device into a folder if its certificate is signed by one of the
//! folder's member users.

use serde::{Deserialize, Serialize};

use crate::{DeviceId, Error, PublicKey, Signed, SignedKind, UserId};

/// "`device` belongs to `user`", signed by `user`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceCertificate {
    /// The owning user.
    pub user: UserId,
    /// The device.
    pub device: DeviceId,
    /// Human-readable device name, e.g. "Office NAS".
    pub name: String,
    /// When the certificate was issued, in milliseconds since the Unix epoch.
    pub issued_ms: i64,
}

impl SignedKind for DeviceCertificate {
    const KIND: &'static str = "identity/1/device-certificate";
}

impl Signed<DeviceCertificate> {
    /// Verifies the signature and checks that it was made by the certificate's user.
    pub fn verify_certificate(&self) -> Result<DeviceCertificate, Error> {
        let certificate = self.verify()?;
        if PublicKey::from(certificate.user) != self.signer {
            return Err(Error::SignerMismatch);
        }
        Ok(certificate)
    }
}

/// "`device` is no longer mine", signed by `user` (ADR 0040). Permanent: a device found again
/// pairs again, with a new key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceRevocation {
    /// The user revoking their device.
    pub user: UserId,
    /// The revoked device.
    pub device: DeviceId,
    /// When it was revoked, in milliseconds since the Unix epoch.
    pub revoked_ms: i64,
    /// When the user lost the device, if they said: its changes from then on are withdrawn.
    pub lost_ms: Option<i64>,
}

impl SignedKind for DeviceRevocation {
    const KIND: &'static str = "identity/1/device-revocation";
}

impl Signed<DeviceRevocation> {
    /// Verifies the signature and checks that it was made by the revocation's user.
    pub fn verify_revocation(&self) -> Result<DeviceRevocation, Error> {
        let revocation = self.verify()?;
        if PublicKey::from(revocation.user) != self.signer {
            return Err(Error::SignerMismatch);
        }
        Ok(revocation)
    }
}

impl DeviceRevocation {
    /// Whether this revokes the device `certificate` is for: same device, and the same user,
    /// so one user cannot revoke another's devices.
    pub fn revokes(&self, certificate: &DeviceCertificate) -> bool {
        self.device == certificate.device && self.user == certificate.user
    }
}
