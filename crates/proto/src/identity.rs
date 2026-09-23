//! Identity messages: device certificates.
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
