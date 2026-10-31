//! Dashboard sessions (§12.3). Two ways in:
//! - manager identity: `/auth/login` → manager approval page → `/auth/callback?code&state` → planner relays the
//!   one-time code exchange (bound to this node and to a nonce kept in a cookie) → role from the manager account;
//! - break-glass (offline): `meduza-np dashboard-link` (root) writes the SHA-256 of a random token into
//!   `<run>/dashboard/login-tokens/`; `/auth/local?token=` consumes it within 10 minutes → admin session.
//!
//! Sessions live in memory only (a dashboard restart signs everyone out), are bounded, and carry a CSRF token that
//! every mutating API call must echo in `X-CSRF-Token`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

pub const SESSION_COOKIE: &str = "mnp_dash";
pub const LOGIN_COOKIE: &str = "mnp_dash_login";
const MAX_SESSIONS: usize = 32;
const LOCAL_TOKEN_TTL: Duration = Duration::from_secs(600);
const LOCAL_SESSION_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Viewer,
    Operator,
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Role::Viewer),
            "operator" => Some(Role::Operator),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Operator => "operator",
            Role::Admin => "admin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub user: String,
    pub role: Role,
    pub csrf: String,
    pub expires: SystemTime,
    /// `manager` or `local`
    pub via: &'static str,
}

#[derive(Default)]
pub struct Sessions {
    map: HashMap<String, Session>,
}

pub fn random_token() -> String {
    let mut b = [0u8; 32];
    getrandom::getrandom(&mut b).expect("OS randomness");
    hex::encode(b)
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

impl Sessions {
    pub fn create(&mut self, user: String, role: Role, ttl: Duration, via: &'static str) -> (String, Session) {
        let now = SystemTime::now();
        self.map.retain(|_, s| s.expires > now);
        if self.map.len() >= MAX_SESSIONS {
            if let Some(oldest) = self.map.iter().min_by_key(|(_, s)| s.expires).map(|(k, _)| k.clone()) {
                self.map.remove(&oldest);
            }
        }
        let token = random_token();
        let s = Session { user, role, csrf: random_token(), expires: now + ttl.min(Duration::from_secs(12 * 3600)), via };
        // only the hash is kept, so a memory dump of the map does not yield usable cookies
        self.map.insert(sha256_hex(&token), s.clone());
        (token, s)
    }

    pub fn get(&mut self, token: &str) -> Option<Session> {
        let key = sha256_hex(token);
        match self.map.get(&key) {
            Some(s) if s.expires > SystemTime::now() => Some(s.clone()),
            Some(_) => {
                self.map.remove(&key);
                None
            }
            None => None,
        }
    }

    pub fn remove(&mut self, token: &str) {
        self.map.remove(&sha256_hex(token));
    }
}

pub fn local_token_dir(run: &Path) -> PathBuf {
    run.join("dashboard").join("login-tokens")
}

/// Consumes a break-glass token: valid once, within [`LOCAL_TOKEN_TTL`] of its creation.
pub fn consume_local_token(run: &Path, token: &str) -> bool {
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let path = local_token_dir(run).join(sha256_hex(token));
    let fresh = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).map(|age| age <= LOCAL_TOKEN_TTL).unwrap_or(false))
        .unwrap_or(false);
    // remove first: a second request with the same token must fail even if the first one is still running
    std::fs::remove_file(&path).is_ok() && fresh
}

pub fn local_session_ttl() -> Duration {
    LOCAL_SESSION_TTL
}

/// Parses `Cookie:` into name → value.
pub fn cookies(header: Option<&str>) -> HashMap<String, String> {
    header
        .unwrap_or_default()
        .split(';')
        .filter_map(|kv| kv.trim().split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

pub fn set_cookie(name: &str, value: &str, max_age: u64, secure: bool) -> String {
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{}", if secure { "; Secure" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_and_local_tokens() {
        let mut s = Sessions::default();
        let (tok, sess) = s.create("alice".into(), Role::Operator, Duration::from_secs(60), "manager");
        assert_eq!(s.get(&tok).unwrap().csrf, sess.csrf);
        assert!(s.get("nope").is_none());
        s.remove(&tok);
        assert!(s.get(&tok).is_none());
        for i in 0..40 {
            s.create(format!("u{i}"), Role::Viewer, Duration::from_secs(60), "local");
        }
        assert!(s.map.len() <= MAX_SESSIONS);

        let run = std::env::temp_dir().join(format!("mnp-dash-auth-{}", std::process::id()));
        let dir = local_token_dir(&run);
        std::fs::create_dir_all(&dir).unwrap();
        let t = random_token();
        std::fs::write(dir.join(sha256_hex(&t)), b"").unwrap();
        assert!(!consume_local_token(&run, "zz"));
        assert!(consume_local_token(&run, &t));
        assert!(!consume_local_token(&run, &t), "single use");
        let _ = std::fs::remove_dir_all(&run);
        assert!(Role::Admin > Role::Operator && Role::Operator > Role::Viewer);
        assert_eq!(cookies(Some("a=1; mnp_dash=xyz")).get("mnp_dash").map(String::as_str), Some("xyz"));
    }
}
