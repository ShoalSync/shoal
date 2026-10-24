//! Shoal `identity` crate: users, devices, folder membership, signing, key formats.
//!
//! - A [`DeviceKey`] is generated on first run. Its public half is the device's `DeviceId`
//!   and also its iroh endpoint ID, so the QUIC handshake authenticates the device.
//! - A [`UserKey`] belongs to a person and signs [`DeviceCertificate`]s for their devices.
//! - A [`FolderMembers`] list names the users who may sync a folder; a device is admitted if it
//!   presents a certificate from one of them.
//!
//! # Key file format (version 1)
//!
//! ```text
//! shoal-device-key 1      (or shoal-user-key 1)
//! <64 lowercase hex digits: the Ed25519 seed>
//! ```
//!
//! Key files are created with mode 0600 on Unix.

mod certificate;
mod keys;
mod membership;
pub mod pairing;

pub use certificate::{decode_certificate, encode_certificate, load_certificate, save_certificate};
pub use keys::{DeviceKey, UserKey};
pub use membership::{AuthError, FolderMembers};
pub use shoal_proto::identity::DeviceCertificate;

/// Identity errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Reading or writing a key file failed.
    #[error("key file I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A key file is not in a format this version understands.
    #[error("malformed key file: {0}")]
    MalformedKey(&'static str),
    /// The operating system could not provide random bytes.
    #[error("no randomness available: {0}")]
    Random(String),
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;
