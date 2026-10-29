//! Authentication: argon2id passwords, random session tokens, the
//! `CurrentUser` / role extractors, CSRF header check and login rate
//! limiting. SPEC §10.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use argon2::password_hash::{self, phc::PasswordHash};
use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts, Method};
use chrono::Utc;
use rand::Rng;
use ss_core::{Role, Session, Store, User};
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::error::{ApiError, ApiResult};
use crate::AppState;

pub const SESSION_COOKIE: &str = "ss_session";
pub const MIN_PASSWORD_LEN: usize = 8;
const MAX_PASSWORD_LEN: usize = 1024;
const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const BLOCK_DURATION: Duration = Duration::from_secs(5 * 60);
/// Argon2 runs on at most this many blocking threads at once.
const ARGON2_CONCURRENCY: usize = 4;
/// Header every cookie-authenticated unsafe request must carry (CSRF guard).
pub const CSRF_HEADER: &str = "x-requested-with";
pub const CSRF_VALUE: &str = "ss";

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
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// A valid argon2id hash of a throwaway password, used to spend the same
/// time on unknown usernames as on real ones.
fn dummy_hash() -> &'static str {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("ss-dummy-password-never-matches").expect("dummy hash"))
}

fn argon2_permits() -> &'static Semaphore {
    static PERMITS: OnceLock<Semaphore> = OnceLock::new();
    PERMITS.get_or_init(|| Semaphore::new(ARGON2_CONCURRENCY))
}

/// Wait for a slot in the argon2 concurrency cap.
async fn argon2_slot() -> ApiResult<SemaphorePermit<'static>> {
    argon2_permits().acquire().await.map_err(ApiError::internal)
}

/// Hash on the blocking pool (argon2 is deliberately slow).
pub async fn hash_password_async(password: String) -> ApiResult<String> {
    let _slot = argon2_slot().await?;
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await?
        .map_err(ApiError::from)
}

pub async fn verify_password_async(password: String, phc: String) -> ApiResult<bool> {
    let _slot = argon2_slot().await?;
    Ok(tokio::task::spawn_blocking(move || verify_password(&password, &phc)).await?)
}

/// Login check. `phc` is `None` for an unknown username: the password is
/// still verified (against the dummy hash) so timing does not reveal which
/// usernames exist. Always false for `None`.
pub async fn verify_login_async(password: String, phc: Option<String>) -> ApiResult<bool> {
    let _slot = argon2_slot().await?;
    Ok(tokio::task::spawn_blocking(move || match phc {
        Some(phc) => verify_password(&password, &phc),
        None => {
            verify_password(&password, dummy_hash());
            false
        }
    })
    .await?)
}

/// Enforce password policy (length only).
pub fn check_password_policy(password: &str) -> ApiResult<()> {
    if password.len() < MIN_PASSWORD_LEN {
        return Err(ApiError::invalid(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    if password.len() > MAX_PASSWORD_LEN {
        return Err(ApiError::invalid("password is too long"));
    }
    Ok(())
}

/// First-run bootstrap: when the user table is empty, create `admin`.
/// Returns the generated password if one was generated (to be shown once).
pub fn bootstrap_admin(
    store: &dyn Store,
    password: Option<&str>,
) -> anyhow::Result<Option<String>> {
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

/// Per-username login limiter (in memory). SPEC §10: after 5 failures
/// within 5 minutes, reject with 429 for 5 minutes.
///
/// An attempt is *reserved* before any password work ([`LoginLimiter::begin`]),
/// and reservations in flight count against the limit, so a burst of parallel
/// guesses cannot all slip past the check before any of them has failed.
#[derive(Default)]
pub struct LoginLimiter {
    inner: Mutex<HashMap<String, Attempts>>,
}

#[derive(Default)]
struct Attempts {
    failures: Vec<Instant>,
    in_flight: usize,
    blocked_until: Option<Instant>,
}

impl Attempts {
    fn push_failure(&mut self, now: Instant) {
        self.failures
            .retain(|f| now.duration_since(*f) < FAILURE_WINDOW);
        self.failures.push(now);
        if self.failures.len() >= MAX_FAILURES {
            self.failures.clear();
            self.blocked_until = Some(now + BLOCK_DURATION);
        }
    }
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

    /// Record a failure without a reservation (used by tests and internally).
    pub fn record_failure(&self, key: &str) {
        let now = Instant::now();
        self.lock()
            .entry(key.to_string())
            .or_default()
            .push_failure(now);
    }

    pub fn record_success(&self, key: &str) {
        self.lock().remove(key);
    }

    /// Reserve one login attempt for `key`. `None` when the key is blocked or
    /// failures plus attempts already in flight reach the limit.
    pub fn begin(self: &Arc<Self>, key: &str) -> Option<LoginAttempt> {
        let now = Instant::now();
        {
            let mut map = self.lock();
            if map.len() > 10_000 {
                map.retain(|_, a| {
                    a.in_flight > 0
                        || a.blocked_until.is_some_and(|u| u > now)
                        || a.failures
                            .iter()
                            .any(|f| now.duration_since(*f) < FAILURE_WINDOW)
                });
            }
            let entry = map.entry(key.to_string()).or_default();
            if entry.blocked_until.is_some_and(|until| until > now) {
                return None;
            }
            entry
                .failures
                .retain(|f| now.duration_since(*f) < FAILURE_WINDOW);
            if entry.failures.len() + entry.in_flight >= MAX_FAILURES {
                return None;
            }
            entry.in_flight += 1;
        }
        Some(LoginAttempt {
            limiter: Arc::clone(self),
            key: key.to_string(),
            done: false,
        })
    }

    fn finish(&self, key: &str, ok: bool) {
        let mut map = self.lock();
        if ok {
            map.remove(key);
            return;
        }
        let entry = map.entry(key.to_string()).or_default();
        entry.in_flight = entry.in_flight.saturating_sub(1);
        entry.push_failure(Instant::now());
    }
}

/// A reserved login attempt. Dropping it without calling
/// [`LoginAttempt::succeeded`] counts as a failure (e.g. a cancelled request).
pub struct LoginAttempt {
    limiter: Arc<LoginLimiter>,
    key: String,
    done: bool,
}

impl LoginAttempt {
    pub fn succeeded(mut self) {
        self.done = true;
        self.limiter.finish(&self.key, true);
    }

    pub fn failed(mut self) {
        self.done = true;
        self.limiter.finish(&self.key, false);
    }
}

impl Drop for LoginAttempt {
    fn drop(&mut self) {
        if !self.done {
            self.limiter.finish(&self.key, false);
        }
    }
}

/// How the session token of a request was presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    /// `Authorization: Bearer` (programmatic clients).
    Bearer,
    /// `ss_session` cookie (the web UI). Subject to the CSRF header check.
    Cookie,
    /// `?token=` query parameter, only on the GET routes in [`query_token_allowed`].
    Query,
}

/// The authenticated user behind a request.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub user: User,
    pub token: String,
    pub source: TokenSource,
}

/// `?token=` is accepted only where `<img>`/EventSource/media links need it:
/// GET of live, snapshot, playback, frame, download and the SSE stream.
pub fn query_token_allowed(method: &Method, path: &str) -> bool {
    if *method != Method::GET {
        return false;
    }
    // Inside the nested `/api/v1` router the URI has the prefix stripped; accept both.
    let rest = path
        .strip_prefix("/api/v1")
        .unwrap_or(path)
        .trim_start_matches('/');
    let is_id = |s: &&str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let segments: Vec<&str> = rest.split('/').collect();
    match segments.as_slice() {
        ["cameras", id, "live" | "snapshot"] => is_id(id),
        ["recordings", id, "stream" | "frame" | "download"] => is_id(id),
        ["events", "stream"] => true,
        _ => false,
    }
}

/// Session token and how it was presented: `Authorization: Bearer`, then the
/// `ss_session` cookie, then (where allowed) `?token=`.
pub fn token_from_parts(parts: &Parts) -> Option<(String, TokenSource)> {
    if let Some(value) = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        if let Some(token) = value.strip_prefix("Bearer ") {
            let token = token.trim();
            if !token.is_empty() {
                return Some((token.to_string(), TokenSource::Bearer));
            }
        }
    }
    if let Some(cookies) = parts
        .headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
    {
        for pair in cookies.split(';') {
            if let Some(value) = pair.trim().strip_prefix("ss_session=") {
                if !value.is_empty() {
                    return Some((value.to_string(), TokenSource::Cookie));
                }
            }
        }
    }
    if !query_token_allowed(&parts.method, parts.uri.path()) {
        return None;
    }
    parts.uri.query().and_then(|q| {
        q.split('&')
            .find_map(|pair| pair.strip_prefix("token=").filter(|v| !v.is_empty()))
            .map(|t| (t.to_string(), TokenSource::Query))
    })
}

/// Resolve a token to a live session and its user.
pub fn authenticate(state: &AppState, token: &str, source: TokenSource) -> ApiResult<CurrentUser> {
    let session = match state.store.get_session(token) {
        Ok(s) => s,
        Err(ss_core::Error::NotFound(_)) => {
            return Err(ApiError::unauthorized("authentication required"))
        }
        Err(e) => return Err(e.into()),
    };
    let user = match state.store.get_user(session.user_id) {
        Ok(u) => u,
        Err(ss_core::Error::NotFound(_)) => {
            return Err(ApiError::unauthorized("authentication required"))
        }
        Err(e) => return Err(e.into()),
    };
    Ok(CurrentUser {
        user,
        token: token.to_string(),
        source,
    })
}

fn is_unsafe(method: &Method) -> bool {
    *method == Method::POST
        || *method == Method::PUT
        || *method == Method::DELETE
        || *method == Method::PATCH
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let (token, source) = token_from_parts(parts)
            .ok_or_else(|| ApiError::unauthorized("authentication required"))?;
        let me = authenticate(state, &token, source)?;
        // Browsers attach the cookie to cross-site form posts; a custom header
        // cannot be set cross-site without CORS, so require it for cookie auth.
        if source == TokenSource::Cookie
            && is_unsafe(&parts.method)
            && parts.headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok()) != Some(CSRF_VALUE)
        {
            return Err(ApiError::csrf_required());
        }
        Ok(me)
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

    #[test]
    fn reservations_in_flight_count_against_the_limit() {
        let l = Arc::new(LoginLimiter::default());
        let held: Vec<_> = (0..5).map(|_| l.begin("bob").expect("reserved")).collect();
        assert!(
            l.begin("bob").is_none(),
            "sixth parallel attempt must be refused"
        );
        assert!(l.begin("alice").is_some());
        for attempt in held {
            attempt.failed();
        }
        assert!(l.begin("bob").is_none(), "five failures block the name");
    }

    #[test]
    fn dropped_attempt_counts_as_failure_and_success_clears() {
        let l = Arc::new(LoginLimiter::default());
        for _ in 0..4 {
            drop(l.begin("bob").unwrap());
        }
        assert!(
            l.begin("bob").is_some(),
            "four dropped attempts leave one slot"
        );
        let l = Arc::new(LoginLimiter::default());
        for _ in 0..5 {
            drop(l.begin("bob").unwrap());
        }
        assert!(l.begin("bob").is_none());

        let l = Arc::new(LoginLimiter::default());
        l.begin("bob").unwrap().failed();
        l.begin("bob").unwrap().succeeded();
        for _ in 0..4 {
            l.begin("bob").unwrap().failed();
        }
        assert!(l.begin("bob").is_some(), "success reset the failure count");
    }

    #[test]
    fn query_token_only_on_allowed_gets() {
        let get = Method::GET;
        assert!(query_token_allowed(&get, "/api/v1/cameras/3/live"));
        assert!(query_token_allowed(&get, "/api/v1/cameras/3/snapshot"));
        assert!(query_token_allowed(&get, "/api/v1/recordings/7/stream"));
        assert!(query_token_allowed(&get, "/api/v1/recordings/7/frame"));
        assert!(query_token_allowed(&get, "/api/v1/recordings/7/download"));
        assert!(query_token_allowed(&get, "/api/v1/events/stream"));
        assert!(
            query_token_allowed(&get, "/cameras/3/live"),
            "nested-router path form"
        );
        assert!(!query_token_allowed(&get, "/api/v1/auth/me"));
        assert!(!query_token_allowed(&get, "/api/v1/cameras"));
        assert!(!query_token_allowed(&get, "/api/v1/cameras/x/live"));
        assert!(!query_token_allowed(
            &Method::POST,
            "/api/v1/cameras/3/live"
        ));
        assert!(!query_token_allowed(
            &Method::DELETE,
            "/api/v1/recordings/7/download"
        ));
    }
}
