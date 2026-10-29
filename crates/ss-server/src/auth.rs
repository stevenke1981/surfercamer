//! Authentication: argon2id passwords, random session tokens, the
//! `CurrentUser` / role extractors and login rate limiting. SPEC §10.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::password_hash::{self, phc::PasswordHash};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts};
use chrono::Utc;
use rand::Rng;
use ss_core::{Role, Session, Store, User};

use crate::error::{ApiError, ApiResult};
use crate::AppState;

pub const SESSION_COOKIE: &str = "ss_session";
pub const MIN_PASSWORD_LEN: usize = 8;
const MAX_PASSWORD_LEN: usize = 1024;
const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const BLOCK_DURATION: Duration = Duration::from_secs(5 * 60);

/// Lowercase hex encoding of `n` random bytes.
pub fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    rand::rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Random alphanumeric string of `len` characters (rejection sampling, no bias).
pub fn random_password(len: usize) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let limit = 256 - (256 % ALPHABET.len());
    let mut out = String::with_capacity(len);
    let mut rng = rand::rng();
    let mut buf = [0u8; 64];
    while out.len() < len {
        rng.fill_bytes(&mut buf);
        for &b in &buf {
            if (b as usize) < limit && out.len() < len {
                out.push(ALPHABET[b as usize % ALPHABET.len()] as char);
            }
        }
    }
    out
}

/// argon2id PHC string for `password`.
pub fn hash_password(password: &str) -> ss_core::Result<String> {
    let salt = password_hash::generate_salt();
    Argon2::default()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| ss_core::Error::Internal(format!("password hashing failed: {e}")))
}

/// True if `password` matches the stored PHC string.
pub fn verify_password(password: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok(),
        Err(_) => false,
    }
}

/// Hash on the blocking pool (argon2 is deliberately slow).
pub async fn hash_password_async(password: String) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await?
        .map_err(ApiError::from)
}

pub async fn verify_password_async(password: String, phc: String) -> ApiResult<bool> {
    Ok(tokio::task::spawn_blocking(move || verify_password(&password, &phc)).await?)
}

/// Enforce password policy (length only).
pub fn check_password_policy(password: &str) -> ApiResult<()> {
    if password.len() < MIN_PASSWORD_LEN {
        return Err(ApiError::invalid(format!("password must be at least {MIN_PASSWORD_LEN} characters")));
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(ApiError::invalid("password is too long"));
    }
    Ok(())
}

/// First-run bootstrap: when the user table is empty, create `admin`.
/// Returns the generated password if one was generated (to be shown once).
pub fn bootstrap_admin(store: &dyn Store, password: Option<&str>) -> anyhow::Result<Option<String>> {
    if store.count_users()? > 0 {
        return Ok(None);
    }
    let (password, generated) = match password {
        Some(p) => {
            if p.len() < MIN_PASSWORD_LEN {
                anyhow::bail!("admin password must be at least {MIN_PASSWORD_LEN} characters");
            }
            (p.to_string(), None)
        }
        None => {
            let p = random_password(16);
            (p.clone(), Some(p))
        }
    };
    let hash = hash_password(&password)?;
    store.create_user("admin", &hash, Role::Admin)?;
    Ok(generated)
}

/// Create a session row for `user` and return it.
pub fn create_session(store: &dyn Store, user: &User) -> ApiResult<Session> {
    let ttl_hours = store.get_settings()?.session_ttl_hours;
    let now = Utc::now();
    let ttl = chrono::TimeDelta::try_hours(i64::from(ttl_hours))
        .ok_or_else(|| ApiError::invalid("session_ttl_hours out of range"))?;
    let session = Session {
        token: random_hex(32),
        user_id: user.id,
        created_at: now,
        expires_at: now + ttl,
    };
    store.create_session(&session)?;
    Ok(session)
}

/// `Set-Cookie` value for the session cookie.
pub fn session_cookie(token: &str, max_age_secs: i64) -> String {
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}")
}

pub fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Per-username failed-login limiter (in memory). SPEC §10: after 5 failures
/// within 5 minutes, reject with 429 for 5 minutes.
#[derive(Default)]
pub struct LoginLimiter {
    inner: Mutex<HashMap<String, Attempts>>,
}

#[derive(Default)]
struct Attempts {
    failures: Vec<Instant>,
    blocked_until: Option<Instant>,
}

impl LoginLimiter {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Attempts>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True if logins for `key` are currently blocked.
    pub fn is_blocked(&self, key: &str) -> bool {
        let now = Instant::now();
        self.lock()
            .get(key)
            .and_then(|a| a.blocked_until)
            .is_some_and(|until| until > now)
    }

    pub fn record_failure(&self, key: &str) {
        let now = Instant::now();
        let mut map = self.lock();
        if map.len() > 10_000 {
            map.retain(|_, a| a.blocked_until.is_some_and(|u| u > now) || a.failures.iter().any(|f| now.duration_since(*f) < FAILURE_WINDOW));
        }
        let entry = map.entry(key.to_string()).or_default();
        entry.failures.retain(|f| now.duration_since(*f) < FAILURE_WINDOW);
        entry.failures.push(now);
        if entry.failures.len() >= MAX_FAILURES {
            entry.failures.clear();
            entry.blocked_until = Some(now + BLOCK_DURATION);
        }
    }

    pub fn record_success(&self, key: &str) {
        self.lock().remove(key);
    }
}

/// The authenticated user behind a request.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub user: User,
    pub token: String,
}

/// Session token from `Authorization: Bearer`, the `ss_session` cookie, or `?token=`.
pub fn token_from_parts(parts: &Parts) -> Option<String> {
    if let Some(value) = parts.headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(token) = value.strip_prefix("Bearer ") {
            let token = token.trim();
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    if let Some(cookies) = parts.headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        for pair in cookies.split(';') {
            if let Some(value) = pair.trim().strip_prefix("ss_session=") {
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    parts.uri.query().and_then(|q| {
        q.split('&')
            .find_map(|pair| pair.strip_prefix("token=").filter(|v| !v.is_empty()))
            .map(str::to_string)
    })
}

/// Resolve a token to a live session and its user.
pub fn authenticate(state: &AppState, token: &str) -> ApiResult<CurrentUser> {
    let session = match state.store.get_session(token) {
        Ok(s) => s,
        Err(ss_core::Error::NotFound(_)) => return Err(ApiError::unauthorized("authentication required")),
        Err(e) => return Err(e.into()),
    };
    let user = match state.store.get_user(session.user_id) {
        Ok(u) => u,
        Err(ss_core::Error::NotFound(_)) => return Err(ApiError::unauthorized("authentication required")),
        Err(e) => return Err(e.into()),
    };
    Ok(CurrentUser { user, token: token.to_string() })
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token = token_from_parts(parts).ok_or_else(|| ApiError::unauthorized("authentication required"))?;
        authenticate(state, &token)
    }
}

macro_rules! role_extractor {
    ($(#[$doc:meta])* $name:ident, $role:expr) => {
        $(#[$doc])*
        pub struct $name(pub CurrentUser);

        impl FromRequestParts<AppState> for $name {
            type Rejection = ApiError;

            async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
                let me = CurrentUser::from_request_parts(parts, state).await?;
                if me.user.role >= $role {
                    Ok(Self(me))
                } else {
                    Err(ApiError::forbidden())
                }
            }
        }
    };
}

role_extractor!(
    /// Any authenticated user (viewer or higher).
    Viewer,
    Role::Viewer
);
role_extractor!(
    /// Manager or admin.
    Manager,
    Role::Manager
);
role_extractor!(
    /// Admin only.
    Admin,
    Role::Admin
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let phc = hash_password("correct horse").unwrap();
        assert!(phc.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &phc));
        assert!(!verify_password("wrong", &phc));
        assert!(!verify_password("x", "not-a-phc"));
    }

    #[test]
    fn random_tokens_have_expected_shape() {
        let t = random_hex(32);
        assert_eq!(t.len(), 64);
        assert!(t.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(random_password(16).len(), 16);
    }

    #[test]
    fn limiter_blocks_after_five_failures() {
        let l = LoginLimiter::default();
        for _ in 0..4 {
            l.record_failure("bob");
            assert!(!l.is_blocked("bob"));
        }
        l.record_failure("bob");
        assert!(l.is_blocked("bob"));
        assert!(!l.is_blocked("alice"));
        l.record_success("bob");
        assert!(!l.is_blocked("bob"));
    }
}
