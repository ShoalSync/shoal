//! Device and user keys.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::Path;

use ed25519_dalek::SigningKey;
use tidefell_proto::identity::{DeviceCertificate, DeviceRevocation};
use tidefell_proto::{DeviceId, Signed, UserId};

use crate::{Error, Result};

fn random_seed() -> Result<[u8; 32]> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| Error::Random(e.to_string()))?;
    Ok(seed)
}

fn encode(header: &str, seed: &[u8; 32]) -> String {
    let hex: String = seed.iter().map(|b| format!("{b:02x}")).collect();
    format!("{header}\n{hex}\n")
}

fn decode(header: &str, text: &str) -> Result<[u8; 32]> {
    let mut lines = text.lines();
    if lines.next() != Some(header) {
        return Err(Error::MalformedKey("unknown header"));
    }
    let hex = lines
        .next()
        .ok_or(Error::MalformedKey("missing key"))?
        .trim();
    if hex.len() != 64 || lines.any(|l| !l.trim().is_empty()) {
        return Err(Error::MalformedKey("bad key length"));
    }
    let mut seed = [0u8; 32];
    for (byte, pair) in seed.iter_mut().zip(hex.as_bytes().as_chunks::<2>().0) {
        let pair = std::str::from_utf8(pair).map_err(|_| Error::MalformedKey("not hex"))?;
        *byte = u8::from_str_radix(pair, 16).map_err(|_| Error::MalformedKey("not hex"))?;
    }
    Ok(seed)
}

fn write_secret(path: &Path, contents: &str) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

macro_rules! key_type {
    ($(#[$doc:meta])* $name:ident, $id:ident, $header:literal) => {
        $(#[$doc])*
        #[derive(Clone)]
        pub struct $name(SigningKey);

        impl $name {
            const HEADER: &'static str = $header;

            /// Generates a new random key.
            pub fn generate() -> Result<Self> {
                Ok(Self::from_seed(random_seed()?))
            }

            /// Builds a key from its 32-byte Ed25519 seed.
            pub fn from_seed(seed: [u8; 32]) -> Self {
                Self(SigningKey::from_bytes(&seed))
            }

            /// The 32-byte Ed25519 seed. Keep it secret.
            pub fn seed(&self) -> [u8; 32] {
                self.0.to_bytes()
            }

            /// The public identifier.
            pub fn id(&self) -> $id {
                $id(self.0.verifying_key().to_bytes())
            }

            /// The signing key, for signing messages.
            pub fn signing_key(&self) -> &SigningKey {
                &self.0
            }

            /// Writes the key to a new file. Fails if the file exists.
            pub fn save(&self, path: &Path) -> Result<()> {
                write_secret(path, &encode(Self::HEADER, &self.seed()))
            }

            /// Reads a key written by `save`.
            pub fn load(path: &Path) -> Result<Self> {
                Ok(Self::from_seed(decode(Self::HEADER, &fs::read_to_string(path)?)?))
            }

            /// Loads the key at `path`, generating and saving a new one if the file does not
            /// exist.
            pub fn load_or_generate(path: &Path) -> Result<Self> {
                match Self::load(path) {
                    Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                        let key = Self::generate()?;
                        key.save(path)?;
                        Ok(key)
                    }
                    other => other,
                }
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.id())
            }
        }
    };
}

key_type!(
    /// A device's key pair. Its public key is the `DeviceId` and the iroh endpoint ID.
    DeviceKey,
    DeviceId,
    "tidefell-device-key 1"
);
key_type!(
    /// A user's key pair, used to certify the user's devices.
    UserKey,
    UserId,
    "tidefell-user-key 1"
);

impl UserKey {
    /// Certifies that `device` belongs to this user.
    pub fn certify(
        &self,
        device: DeviceId,
        name: impl Into<String>,
        issued_ms: i64,
    ) -> Signed<DeviceCertificate> {
        Signed::sign(
            &DeviceCertificate {
                user: self.id(),
                device,
                name: name.into(),
                issued_ms,
            },
            &self.0,
        )
    }

    /// Revokes this user's `device` for good (ADR 0040), lost at `lost_ms` if the user said.
    pub fn revoke(
        &self,
        device: DeviceId,
        revoked_ms: i64,
        lost_ms: Option<i64>,
    ) -> Signed<DeviceRevocation> {
        Signed::sign(
            &DeviceRevocation {
                user: self.id(),
                device,
                revoked_ms,
                lost_ms,
            },
            &self.0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_keys_differ() {
        let a = DeviceKey::generate().unwrap();
        let b = DeviceKey::generate().unwrap();
        assert_ne!(a.id(), b.id());
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        let key = DeviceKey::from_seed([5; 32]);
        key.save(&path).unwrap();
        assert_eq!(DeviceKey::load(&path).unwrap().id(), key.id());
        assert!(key.save(&path).is_err(), "never overwrites a key");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn load_or_generate_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user.key");
        let first = UserKey::load_or_generate(&path).unwrap();
        let second = UserKey::load_or_generate(&path).unwrap();
        assert_eq!(first.id(), second.id());
    }

    #[test]
    fn rejects_wrong_or_malformed_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("device.key");
        DeviceKey::from_seed([1; 32]).save(&path).unwrap();
        assert!(matches!(UserKey::load(&path), Err(Error::MalformedKey(_))));

        let bad = dir.path().join("bad.key");
        fs::write(&bad, "tidefell-user-key 1\nzz\n").unwrap();
        assert!(matches!(UserKey::load(&bad), Err(Error::MalformedKey(_))));
    }
}
