//! A device's identity files in its data directory, shared by `tidefelld` and the mobile apps.
//!
//! ```text
//! <data>/device.key    this device's key (created on first use)
//! <data>/device.cert   its certificate, once certified or paired
//! <data>/user.key      the user key, only on devices that approve new ones
//! ```

use std::path::{Path, PathBuf};

use tidefell_identity::{DeviceKey, UserKey};
use tidefell_proto::identity::DeviceCertificate;
use tidefell_proto::Signed;

use crate::{Error, Result};

/// Where the device key lives.
pub fn device_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("device.key")
}

/// Where the device certificate lives.
pub fn certificate_path(data_dir: &Path) -> PathBuf {
    data_dir.join("device.cert")
}

/// Where a user key lives on a device that holds one.
pub fn user_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("user.key")
}

fn identity_err(path: &Path) -> impl FnOnce(tidefell_identity::Error) -> Error + '_ {
    move |e| Error::Invalid(format!("{}: {e}", path.display()))
}

/// Loads this device's key, creating it (and the data directory) on first use.
pub fn device_key(data_dir: &Path) -> Result<DeviceKey> {
    std::fs::create_dir_all(data_dir).map_err(crate::io_err(data_dir))?;
    let path = device_key_path(data_dir);
    DeviceKey::load_or_generate(&path).map_err(identity_err(&path))
}

/// Loads the certificate for `key`, if the device has one. Fails if it is invalid or for
/// another device.
pub fn certificate(data_dir: &Path, key: &DeviceKey) -> Result<Option<Signed<DeviceCertificate>>> {
    let path = certificate_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }
    let certificate = tidefell_identity::load_certificate(&path).map_err(identity_err(&path))?;
    let body = certificate
        .verify_certificate()
        .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
    if body.device != key.id() {
        return Err(Error::Invalid(format!(
            "{} is for another device",
            path.display()
        )));
    }
    Ok(Some(certificate))
}

/// Stores this device's certificate.
pub fn save_certificate(data_dir: &Path, certificate: &Signed<DeviceCertificate>) -> Result<()> {
    let path = certificate_path(data_dir);
    tidefell_identity::save_certificate(&path, certificate).map_err(identity_err(&path))
}

/// Loads the user key, if this device holds one.
pub fn user_key(data_dir: &Path) -> Result<Option<UserKey>> {
    let path = user_key_path(data_dir);
    if !path.exists() {
        return Ok(None);
    }
    UserKey::load(&path).map(Some).map_err(identity_err(&path))
}
