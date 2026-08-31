use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// Name of the browser session cookie.
pub const SESSION_COOKIE: &str = "carson_session";
/// How long a session lives without activity (sliding, ms).
pub const SESSION_TTL_MS: u64 = 24 * 60 * 60 * 1000;
/// How long the session cookie persists in the browser (ms, ~30 days), so a
/// user only logs in once per browser rather than every refresh.
pub const SESSION_COOKIE_MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// In-memory login sessions: an opaque id per authenticated browser, kept on
/// `AppState`. Nothing here is persisted; a server restart logs everyone out.
pub struct AuthState {
    /// session id -> expiry (ms since epoch). Validation slides the expiry.
    sessions: Mutex<HashMap<String, u64>>,
    /// Timestamps of recent failed logins, used to throttle brute force.
    failures: Mutex<VecDeque<u64>>,
    /// Max failed logins inside `FAILURE_WINDOW_MS` before login is rejected.
    max_failures: usize,
    /// Window over which failures are counted (ms).
    failure_window_ms: u64,
}

impl AuthState {
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(VecDeque::new()),
            max_failures: 5,
            failure_window_ms: 60 * 1000,
        }
    }

    /// Create a session and return its opaque id.
    pub fn issue(&self) -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let expires = now_ms() + SESSION_TTL_MS;
        self.sessions.lock().unwrap().insert(id.clone(), expires);
        id
    }

    /// A session is valid if present and unexpired; validity slides the
    /// expiry so a day of activity never logs the user out.
    pub fn validate(&self, id: &str) -> bool {
        let now = now_ms();
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(id) {
            Some(expires) if *expires > now => {
                sessions.insert(id.to_string(), now + SESSION_TTL_MS);
                true
            }
            _ => {
                sessions.remove(id);
                false
            }
        }
    }

    pub fn revoke(&self, id: &str) {
        self.sessions.lock().unwrap().remove(id);
    }

    /// Prune expired sessions so an idle server doesn't accumulate them.
    pub fn sweep(&self) {
        let now = now_ms();
        let mut sessions = self.sessions.lock().unwrap();
        sessions.retain(|_, expires| *expires > now);
    }

    /// True when too many logins failed recently; gates the login endpoint.
    pub fn is_throttled(&self) -> bool {
        let now = now_ms();
        let mut failures = self.failures.lock().unwrap();
        while failures
            .front()
            .is_some_and(|t| now.saturating_sub(*t) > self.failure_window_ms)
        {
            failures.pop_front();
        }
        failures.len() >= self.max_failures
    }

    /// Record a failed login attempt.
    pub fn record_failure(&self) {
        self.failures.lock().unwrap().push_back(now_ms());
    }

    /// Forget failed logins after a successful one.
    pub fn clear_failures(&self) {
        self.failures.lock().unwrap().clear();
    }
}

impl Default for AuthState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_issues_validates_and_revokes() {
        let auth = AuthState::new();
        let id = auth.issue();
        assert!(auth.validate(&id));
        assert!(auth.validate(&id));
        auth.revoke(&id);
        assert!(!auth.validate(&id));
    }

    #[test]
    fn unknown_session_is_invalid() {
        let auth = AuthState::new();
        assert!(!auth.validate("nope"));
    }

    #[test]
    fn throttle_blocks_after_max_failures() {
        let auth = AuthState::new();
        for _ in 0..5 {
            assert!(!auth.is_throttled());
            auth.record_failure();
        }
        assert!(auth.is_throttled());
        auth.clear_failures();
        assert!(!auth.is_throttled());
    }
}
