//! Web accounts: local logins mapped to Tidefell users (ADR 0011).
//!
//! Stored as JSON in `<data_dir>/web/accounts.json` so administrators can audit them. The file
//! is re-read on every login, so accounts added or removed with `tidefelld web account ...`
//! apply without a restart.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use serde::{Deserialize, Serialize};
use tidefell_proto::UserId;

/// Account errors.
#[derive(Debug, thiserror::Error)]
pub enum AccountError {
    /// Reading or writing the accounts file failed.
    #[error("accounts file {path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The accounts file is not valid.
    #[error("accounts file {0} is corrupt")]
    Corrupt(PathBuf),
    /// The request is invalid.
    #[error("{0}")]
    Invalid(String),
}

/// One account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Login name: 1–64 of `a-z 0-9 . _ -`.
    pub username: String,
    /// Argon2id PHC string.
    pub password_hash: String,
    /// The Tidefell user this account acts for (hex).
    pub user: String,
    /// Administrators can manage accounts in the web UI.
    pub admin: bool,
}

impl Account {
    /// The linked user.
    pub fn user_id(&self) -> Option<UserId> {
        self.user.parse().ok()
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    accounts: Vec<Account>,
}

/// The accounts of one web peer.
#[derive(Debug, Clone)]
pub struct AccountStore {
    path: PathBuf,
}

fn valid_username(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

impl AccountStore {
    /// The store under a daemon's data directory.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("web").join("accounts.json"),
        }
    }

    fn io(&self) -> impl FnOnce(std::io::Error) -> AccountError + '_ {
        move |source| AccountError::Io {
            path: self.path.clone(),
            source,
        }
    }

    fn load(&self) -> Result<File, AccountError> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|_| AccountError::Corrupt(self.path.clone()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
            Err(e) => Err(self.io()(e)),
        }
    }

    fn save(&self, file: &File) -> Result<(), AccountError> {
        let dir = self.path.parent().expect("accounts file has a directory");
        fs::create_dir_all(dir).map_err(self.io())?;
        let tmp = self.path.with_extension("json.tmp");
        {
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut out = options.open(&tmp).map_err(self.io())?;
            let json = serde_json::to_vec_pretty(file).expect("accounts serialise");
            out.write_all(&json).map_err(self.io())?;
            out.sync_all().map_err(self.io())?;
        }
        fs::rename(&tmp, &self.path).map_err(self.io())?;
        // So the rename survives a power cut (ADR 0034), where the filesystem allows it.
        #[cfg(unix)]
        if let Some(dir) = self.path.parent() {
            let _ = fs::File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    }

    /// All accounts.
    pub fn list(&self) -> Result<Vec<Account>, AccountError> {
        Ok(self.load()?.accounts)
    }

    /// Adds an account, or replaces the password, user and role of an existing one.
    pub fn upsert(
        &self,
        username: &str,
        password: &str,
        user: UserId,
        admin: bool,
    ) -> Result<(), AccountError> {
        if !valid_username(username) {
            return Err(AccountError::Invalid(
                "usernames are 1-64 characters of a-z, 0-9, '.', '_' and '-'".into(),
            ));
        }
        if password.chars().count() < 10 {
            return Err(AccountError::Invalid(
                "passwords must be at least 10 characters".into(),
            ));
        }
        let password_hash = Argon2::default()
            .hash_password(password.as_bytes())
            .map_err(|e| AccountError::Invalid(e.to_string()))?
            .to_string();
        let mut file = self.load()?;
        file.accounts.retain(|a| a.username != username);
        file.accounts.push(Account {
            username: username.to_string(),
            password_hash,
            user: user.to_string(),
            admin,
        });
        file.accounts.sort_by(|a, b| a.username.cmp(&b.username));
        self.save(&file)
    }

    /// Removes an account. Returns false if it did not exist.
    pub fn remove(&self, username: &str) -> Result<bool, AccountError> {
        let mut file = self.load()?;
        let before = file.accounts.len();
        file.accounts.retain(|a| a.username != username);
        let removed = file.accounts.len() != before;
        if removed {
            self.save(&file)?;
        }
        Ok(removed)
    }

    /// The account, if `password` is correct for `username`.
    pub fn verify(&self, username: &str, password: &str) -> Result<Option<Account>, AccountError> {
        let accounts = self.load()?.accounts;
        let Some(account) = accounts.into_iter().find(|a| a.username == username) else {
            // Spend comparable time so response timing does not reveal which names exist.
            let _ = Argon2::default().hash_password(password.as_bytes());
            return Ok(None);
        };
        let ok = PasswordHash::new(&account.password_hash).is_ok_and(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        });
        Ok(ok.then_some(account))
    }

    /// The account named `username`, if it still exists.
    pub fn get(&self, username: &str) -> Result<Option<Account>, AccountError> {
        Ok(self
            .load()?
            .accounts
            .into_iter()
            .find(|a| a.username == username))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(n: u8) -> UserId {
        UserId([n; 32])
    }

    #[test]
    fn add_verify_remove() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());
        store
            .upsert("alice", "correct horse battery", user(1), true)
            .unwrap();
        let account = store
            .verify("alice", "correct horse battery")
            .unwrap()
            .unwrap();
        assert_eq!(account.user_id(), Some(user(1)));
        assert!(account.admin);
        assert!(account.password_hash.starts_with("$argon2id$"));
        assert!(store.verify("alice", "wrong password!!").unwrap().is_none());
        assert!(store
            .verify("mallory", "correct horse battery")
            .unwrap()
            .is_none());

        // Replacing keeps one account.
        store
            .upsert("alice", "another long password", user(2), false)
            .unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(store
            .verify("alice", "correct horse battery")
            .unwrap()
            .is_none());

        assert!(store.remove("alice").unwrap());
        assert!(!store.remove("alice").unwrap());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("web/accounts.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn rejects_bad_names_and_short_passwords() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::new(dir.path());
        assert!(store
            .upsert("Alice", "long enough password", user(1), false)
            .is_err());
        assert!(store
            .upsert("a/b", "long enough password", user(1), false)
            .is_err());
        assert!(store.upsert("alice", "short", user(1), false).is_err());
    }
}
