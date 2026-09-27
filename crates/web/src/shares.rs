//! Share links (ADR 0019): access to one file or directory without an account. A link to a
//! directory can instead be upload-only (ADR 0029): visitors can add files but see none.
//!
//! Stored as JSON in `<data_dir>/web/shares.json`, readable only by the daemon's user. Only a
//! BLAKE3 hash of each link's token is stored, so the file does not reveal working links. A
//! link has a separate short ID that appears in logs and in the list of links.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use serde::{Deserialize, Serialize};

use crate::accounts::AccountError;
use crate::sessions::same_secret;

/// How long a correct link password is remembered by the browser that entered it.
pub(crate) const UNLOCK_TTL: Duration = Duration::from_secs(60 * 60);

/// One share link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Share {
    /// Short identifier for logs and revocation; not a secret.
    pub(crate) id: String,
    /// BLAKE3 hash of the token in the link's URL (hex).
    pub(crate) token_hash: String,
    /// The folder (hex).
    pub(crate) folder: String,
    /// The shared file, or the directory prefix ("" for the whole folder).
    pub(crate) path: String,
    /// True if `path` is a directory.
    pub(crate) directory: bool,
    /// The account that created the link. The link stops working if the account is removed
    /// or its user loses access to the folder.
    pub(crate) created_by: String,
    /// Milliseconds since the Unix epoch.
    pub(crate) created_at_ms: i64,
    /// When the link stops working, if ever.
    pub(crate) expires_at_ms: Option<i64>,
    /// Argon2id PHC string, if the link needs a password.
    pub(crate) password_hash: Option<String>,
    /// How many downloads the link allows, if limited.
    pub(crate) max_downloads: Option<u32>,
    /// Downloads so far.
    pub(crate) downloads: u32,
    /// True if visitors can only upload into the directory, and see none of its contents.
    #[serde(default)]
    pub(crate) upload_only: bool,
    /// How many bytes an upload-only link accepts in total, if limited.
    #[serde(default)]
    pub(crate) max_upload_bytes: Option<u64>,
    /// How many files an upload-only link accepts, if limited.
    #[serde(default)]
    pub(crate) max_uploads: Option<u32>,
    /// Bytes uploaded so far.
    #[serde(default)]
    pub(crate) uploaded_bytes: u64,
    /// Files uploaded so far.
    #[serde(default)]
    pub(crate) uploads: u32,
}

impl Share {
    /// True if the link has expired at `now_ms`.
    pub(crate) fn expired(&self, now_ms: i64) -> bool {
        self.expires_at_ms.is_some_and(|t| t <= now_ms)
    }

    /// True if the link's limit is used up: downloads, or for an upload-only link, files or
    /// bytes.
    pub(crate) fn exhausted(&self) -> bool {
        if self.upload_only {
            self.max_uploads.is_some_and(|max| self.uploads >= max)
                || self.remaining_bytes() == Some(0)
        } else {
            self.max_downloads.is_some_and(|max| self.downloads >= max)
        }
    }

    /// How many more bytes an upload-only link accepts, if limited.
    pub(crate) fn remaining_bytes(&self) -> Option<u64> {
        self.max_upload_bytes
            .map(|max| max.saturating_sub(self.uploaded_bytes))
    }

    /// True if `password` unlocks the link.
    pub(crate) fn password_ok(&self, password: &str) -> bool {
        match &self.password_hash {
            None => true,
            Some(hash) => PasswordHash::new(hash).is_ok_and(|h| {
                Argon2::default()
                    .verify_password(password.as_bytes(), &h)
                    .is_ok()
            }),
        }
    }
}

/// What a new link shares, and on what terms.
pub(crate) struct NewShare {
    pub(crate) folder: String,
    pub(crate) path: String,
    pub(crate) directory: bool,
    pub(crate) created_by: String,
    pub(crate) created_at_ms: i64,
    pub(crate) expires_at_ms: Option<i64>,
    pub(crate) password: Option<String>,
    pub(crate) max_downloads: Option<u32>,
    pub(crate) upload_only: bool,
    pub(crate) max_upload_bytes: Option<u64>,
    pub(crate) max_uploads: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    shares: Vec<Share>,
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("the OS provides randomness");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// The share links of one web peer.
pub(crate) struct ShareStore {
    path: PathBuf,
    /// Serialises read-modify-write cycles, such as counting a download.
    lock: Mutex<()>,
    /// Browsers that entered a link's password: unlock token → (link ID, expiry).
    unlocked: Mutex<HashMap<String, (String, Instant)>>,
}

impl ShareStore {
    /// The store under a daemon's data directory.
    pub(crate) fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("web").join("shares.json"),
            lock: Mutex::new(()),
            unlocked: Mutex::new(HashMap::new()),
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
        let dir = self.path.parent().expect("shares file has a directory");
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
            let json = serde_json::to_vec_pretty(file).expect("shares serialise");
            out.write_all(&json).map_err(self.io())?;
            out.sync_all().map_err(self.io())?;
        }
        fs::rename(&tmp, &self.path).map_err(self.io())
    }

    /// Creates a link and returns it with the token for its URL. The token is not stored and
    /// cannot be shown again.
    pub(crate) fn create(&self, new: NewShare) -> Result<(Share, String), AccountError> {
        let password_hash = match new.password {
            Some(password) => Some(
                Argon2::default()
                    .hash_password(password.as_bytes())
                    .map_err(|e| AccountError::Invalid(e.to_string()))?
                    .to_string(),
            ),
            None => None,
        };
        // 128 bits (ADR 0019).
        let token = random_hex(16);
        let share = Share {
            id: random_hex(6),
            token_hash: token_hash(&token),
            folder: new.folder,
            path: new.path,
            directory: new.directory,
            created_by: new.created_by,
            created_at_ms: new.created_at_ms,
            expires_at_ms: new.expires_at_ms,
            password_hash,
            max_downloads: new.max_downloads,
            downloads: 0,
            upload_only: new.upload_only,
            max_upload_bytes: new.max_upload_bytes,
            max_uploads: new.max_uploads,
            uploaded_bytes: 0,
            uploads: 0,
        };
        let _guard = self.lock.lock().unwrap();
        let mut file = self.load()?;
        file.shares.push(share.clone());
        self.save(&file)?;
        Ok((share, token))
    }

    /// All links, newest first.
    pub(crate) fn list(&self) -> Result<Vec<Share>, AccountError> {
        let mut shares = self.load()?.shares;
        shares.sort_by_key(|s| std::cmp::Reverse(s.created_at_ms));
        Ok(shares)
    }

    /// The link whose URL carries `token`.
    pub(crate) fn find(&self, token: &str) -> Result<Option<Share>, AccountError> {
        let hash = token_hash(token);
        Ok(self
            .load()?
            .shares
            .into_iter()
            .find(|s| same_secret(&s.token_hash, &hash)))
    }

    /// Removes a link. Returns it, or `None` if there was no such link.
    pub(crate) fn revoke(&self, id: &str) -> Result<Option<Share>, AccountError> {
        let _guard = self.lock.lock().unwrap();
        let mut file = self.load()?;
        let Some(index) = file.shares.iter().position(|s| s.id == id) else {
            return Ok(None);
        };
        let share = file.shares.remove(index);
        self.save(&file)?;
        self.unlocked
            .lock()
            .unwrap()
            .retain(|_, (link, _)| *link != share.id);
        Ok(Some(share))
    }

    /// Counts one download of link `id`. Returns false, without counting, if the link is gone
    /// or its limit is used up.
    pub(crate) fn record_download(&self, id: &str) -> Result<bool, AccountError> {
        let _guard = self.lock.lock().unwrap();
        let mut file = self.load()?;
        let Some(share) = file.shares.iter_mut().find(|s| s.id == id) else {
            return Ok(false);
        };
        if share.exhausted() {
            return Ok(false);
        }
        share.downloads += 1;
        self.save(&file)?;
        Ok(true)
    }

    /// Counts one upload of `bytes` to link `id`. Returns false, without counting, if the link
    /// is gone or the upload would go over its limits.
    pub(crate) fn record_upload(&self, id: &str, bytes: u64) -> Result<bool, AccountError> {
        let _guard = self.lock.lock().unwrap();
        let mut file = self.load()?;
        let Some(share) = file.shares.iter_mut().find(|s| s.id == id) else {
            return Ok(false);
        };
        let too_many = share.max_uploads.is_some_and(|max| share.uploads >= max);
        if too_many || share.remaining_bytes().is_some_and(|left| bytes > left) {
            return Ok(false);
        }
        share.uploads += 1;
        share.uploaded_bytes += bytes;
        self.save(&file)?;
        Ok(true)
    }

    /// Remembers that a browser entered the password of link `id`, and returns the value for
    /// its cookie.
    pub(crate) fn unlock(&self, id: &str) -> String {
        let token = random_hex(32);
        let mut unlocked = self.unlocked.lock().unwrap();
        let now = Instant::now();
        unlocked.retain(|_, (_, expires)| *expires > now);
        unlocked.insert(token.clone(), (id.to_string(), now + UNLOCK_TTL));
        token
    }

    /// True if `cookie` unlocks link `id`.
    pub(crate) fn is_unlocked(&self, id: &str, cookie: &str) -> bool {
        self.unlocked
            .lock()
            .unwrap()
            .get(cookie)
            .is_some_and(|(link, expires)| link == id && *expires > Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_share(password: Option<&str>, max_downloads: Option<u32>) -> NewShare {
        NewShare {
            folder: "ab".repeat(32),
            path: "report.pdf".into(),
            directory: false,
            created_by: "alice".into(),
            created_at_ms: 1_000,
            expires_at_ms: Some(2_000),
            password: password.map(Into::into),
            max_downloads,
            upload_only: false,
            max_upload_bytes: None,
            max_uploads: None,
        }
    }

    #[test]
    fn create_find_count_revoke() {
        let dir = tempfile::tempdir().unwrap();
        let store = ShareStore::new(dir.path());
        let (share, token) = store
            .create(new_share(Some("open sesame"), Some(2)))
            .unwrap();
        assert_eq!(token.len(), 32);

        // Only the hash is stored.
        let json = fs::read_to_string(dir.path().join("web/shares.json")).unwrap();
        assert!(!json.contains(&token));
        assert!(json.contains(&share.token_hash));

        let found = store.find(&token).unwrap().unwrap();
        assert_eq!(found.id, share.id);
        assert!(store.find(&"0".repeat(32)).unwrap().is_none());
        assert!(found.password_ok("open sesame"));
        assert!(!found.password_ok("guess"));
        assert!(!found.expired(1_999) && found.expired(2_000));

        assert!(store.record_download(&share.id).unwrap());
        assert!(store.record_download(&share.id).unwrap());
        assert!(!store.record_download(&share.id).unwrap(), "limit of 2");
        assert!(store.find(&token).unwrap().unwrap().exhausted());

        let cookie = store.unlock(&share.id);
        assert!(store.is_unlocked(&share.id, &cookie));
        assert!(!store.is_unlocked("other", &cookie));

        assert_eq!(store.revoke(&share.id).unwrap().unwrap().id, share.id);
        assert!(store.revoke(&share.id).unwrap().is_none());
        assert!(store.find(&token).unwrap().is_none());
        assert!(!store.is_unlocked(&share.id, &cookie));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.path().join("web/shares.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn links_without_password_or_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = ShareStore::new(dir.path());
        let (share, _) = store.create(new_share(None, None)).unwrap();
        assert!(share.password_ok(""));
        for _ in 0..5 {
            assert!(store.record_download(&share.id).unwrap());
        }
        let listed = store.list().unwrap();
        assert_eq!(listed[0].downloads, 5);
        assert!(!listed[0].exhausted());
    }

    #[test]
    fn upload_limits() {
        let dir = tempfile::tempdir().unwrap();
        let store = ShareStore::new(dir.path());
        let (share, _) = store
            .create(NewShare {
                path: "inbox".into(),
                directory: true,
                upload_only: true,
                max_upload_bytes: Some(100),
                max_uploads: Some(3),
                ..new_share(None, None)
            })
            .unwrap();
        assert!(store.record_upload(&share.id, 60).unwrap());
        assert!(
            !store.record_upload(&share.id, 41).unwrap(),
            "over 100 bytes"
        );
        assert!(store.record_upload(&share.id, 40).unwrap());
        let listed = store.list().unwrap();
        assert_eq!((listed[0].uploads, listed[0].uploaded_bytes), (2, 100));
        assert!(listed[0].exhausted(), "no bytes left");
        assert!(store.record_upload(&share.id, 0).unwrap());
        assert!(
            !store.record_upload(&share.id, 0).unwrap(),
            "limit of 3 files"
        );
        assert!(!store.record_upload("missing", 0).unwrap());

        // Links saved before upload-only links existed still load.
        let json = fs::read_to_string(dir.path().join("web/shares.json")).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        for key in [
            "upload_only",
            "max_upload_bytes",
            "max_uploads",
            "uploaded_bytes",
            "uploads",
        ] {
            value["shares"][0].as_object_mut().unwrap().remove(key);
        }
        fs::write(dir.path().join("web/shares.json"), value.to_string()).unwrap();
        let old = &store.list().unwrap()[0];
        assert!(!old.upload_only && old.uploads == 0);
    }
}
