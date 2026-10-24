//! Where the app keeps the device key (ADR 0038): in a vault of its own, such as one sealed with
//! an Android Keystore key, so a copy of the app's storage holds no usable device key.

use std::path::Path;
use std::sync::RwLock;

use shoal_engine::device;
use shoal_engine::DeviceKey;

use crate::{Result, ShoalError};

/// Keeps the device key's 32-byte seed for the library. The app implements it.
#[uniffi::export(callback_interface)]
pub trait KeyVault: Send + Sync {
    /// The seed stored earlier, or none if nothing is stored. Fails, rather than returning none,
    /// if a seed is stored but cannot be read: a new key would make this a different device.
    fn load(&self) -> std::result::Result<Option<Vec<u8>>, ShoalError>;
    /// Stores `seed`, replacing nothing: it is called only when nothing is stored.
    fn store(&self, seed: Vec<u8>) -> std::result::Result<(), ShoalError>;
}

static VAULT: RwLock<Option<Box<dyn KeyVault>>> = RwLock::new(None);

/// Keeps the device key in `vault` from now on, for every data directory. Call it before
/// anything that uses the device key. A key already in a data directory's `device.key` file is
/// moved into the vault the first time it is needed, and the file deleted.
#[uniffi::export]
pub fn set_key_vault(vault: Box<dyn KeyVault>) {
    *VAULT.write().unwrap() = Some(vault);
}

/// This device's key: from the vault if one is set, else from the data directory's file. With a
/// vault, a key file left from before is moved into it, and a new key is made only when neither
/// holds one.
pub(crate) fn device_key(data_dir: &Path) -> Result<DeviceKey> {
    let vault = VAULT.read().unwrap();
    let Some(vault) = vault.as_deref() else {
        return Ok(device::device_key(data_dir)?);
    };
    from_vault(vault, data_dir)
}

fn from_vault(vault: &dyn KeyVault, data_dir: &Path) -> Result<DeviceKey> {
    if let Some(seed) = vault.load()? {
        return seed_key(&seed);
    }
    let file = device::device_key_path(data_dir);
    let key = if file.exists() {
        DeviceKey::load(&file).map_err(|e| failed(format!("{}: {e}", file.display())))?
    } else {
        DeviceKey::generate().map_err(|e| failed(e.to_string()))?
    };
    vault.store(key.seed().to_vec())?;
    // Only once the vault gives it back is the file's copy gone.
    let stored = vault
        .load()?
        .ok_or_else(|| failed("the key vault lost the key"))?;
    if seed_key(&stored)?.id() != key.id() {
        return Err(failed("the key vault returned another key"));
    }
    if file.exists() {
        remove_key_file(&file)?;
    }
    Ok(key)
}

fn seed_key(seed: &[u8]) -> Result<DeviceKey> {
    let seed: [u8; 32] = seed
        .try_into()
        .map_err(|_| failed("the key vault holds a key of the wrong length"))?;
    Ok(DeviceKey::from_seed(seed))
}

/// Overwrites the key file before deleting it, so its blocks do not keep the key where the
/// filesystem writes in place.
fn remove_key_file(file: &Path) -> Result<()> {
    use std::io::Write;
    let io = |e: std::io::Error| failed(format!("{}: {e}", file.display()));
    let len = std::fs::metadata(file).map_err(io)?.len();
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(file)
        .map_err(io)?;
    f.write_all(&vec![0; len as usize]).map_err(io)?;
    f.sync_all().map_err(io)?;
    drop(f);
    std::fs::remove_file(file).map_err(io)
}

fn failed(detail: impl Into<String>) -> ShoalError {
    ShoalError::Failed {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Memory(Mutex<Option<Vec<u8>>>);

    impl KeyVault for Memory {
        fn load(&self) -> std::result::Result<Option<Vec<u8>>, ShoalError> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn store(&self, seed: Vec<u8>) -> std::result::Result<(), ShoalError> {
            *self.0.lock().unwrap() = Some(seed);
            Ok(())
        }
    }

    struct Broken;

    impl KeyVault for Broken {
        fn load(&self) -> std::result::Result<Option<Vec<u8>>, ShoalError> {
            Err(failed("the keystore is locked"))
        }
        fn store(&self, _: Vec<u8>) -> std::result::Result<(), ShoalError> {
            Err(failed("the keystore is locked"))
        }
    }

    #[test]
    fn a_new_key_lives_only_in_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let vault = Memory::default();
        let key = from_vault(&vault, dir.path()).unwrap();
        assert!(!device::device_key_path(dir.path()).exists());
        assert_eq!(from_vault(&vault, dir.path()).unwrap().id(), key.id());
    }

    #[test]
    fn a_key_file_moves_into_the_vault() {
        let dir = tempfile::tempdir().unwrap();
        let before = device::device_key(dir.path()).unwrap();
        let vault = Memory::default();
        assert_eq!(from_vault(&vault, dir.path()).unwrap().id(), before.id());
        assert!(!device::device_key_path(dir.path()).exists());
        assert_eq!(from_vault(&vault, dir.path()).unwrap().id(), before.id());
    }

    #[test]
    fn a_vault_that_cannot_be_read_is_an_error_not_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let before = device::device_key(dir.path()).unwrap();
        assert!(from_vault(&Broken, dir.path()).is_err());
        // The file is untouched.
        assert_eq!(device::device_key(dir.path()).unwrap().id(), before.id());
    }
}
