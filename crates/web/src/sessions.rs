//! In-memory sessions. Restarting the daemon signs everyone out.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use shoal_proto::UserId;

/// How long a session lasts.
pub(crate) const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// A signed-in account.
#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub(crate) username: String,
    pub(crate) user: UserId,
    pub(crate) admin: bool,
    /// Token that every form must echo back.
    pub(crate) csrf: String,
    expires: Instant,
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the OS provides randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time string comparison for secrets.
pub(crate) fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[derive(Default)]
pub(crate) struct Sessions {
    by_token: Mutex<HashMap<String, Session>>,
}

impl Sessions {
    /// Starts a session and returns its token.
    pub(crate) fn create(&self, username: String, user: UserId, admin: bool) -> String {
        let token = random_token();
        let session = Session {
            username,
            user,
            admin,
            csrf: random_token(),
            expires: Instant::now() + SESSION_TTL,
        };
        let mut sessions = self.by_token.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        sessions.insert(token.clone(), session);
        token
    }

    pub(crate) fn get(&self, token: &str) -> Option<Session> {
        let sessions = self.by_token.lock().unwrap();
        sessions
            .get(token)
            .filter(|s| s.expires > Instant::now())
            .cloned()
    }

    pub(crate) fn remove(&self, token: &str) {
        self.by_token.lock().unwrap().remove(token);
    }

    /// Ends every session of `username`, e.g. after the account was removed.
    pub(crate) fn remove_user(&self, username: &str) {
        self.by_token
            .lock()
            .unwrap()
            .retain(|_, s| s.username != username);
    }
}
