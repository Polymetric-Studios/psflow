use std::collections::HashMap;
use std::sync::Mutex;

/// Per-run auth state, attached to [`crate::execute::ExecutionContext`].
///
/// Holds mutable state that auth strategies need across requests within a
/// single graph run — most notably cookie jars. Scoped per
/// `ExecutionContext` so no cross-run cookie bleed.
#[derive(Debug, Default)]
pub struct AuthState {
    /// Keyed by strategy name. Each strategy instance that needs state
    /// gets its own jar.
    jars: Mutex<HashMap<String, CookieJar>>,
}

impl AuthState {
    pub fn new() -> Self {
        Self {
            jars: Mutex::new(HashMap::new()),
        }
    }

    /// Atomically mutate the jar for `strategy_name`, creating it if missing.
    pub fn with_jar<R>(&self, strategy_name: &str, f: impl FnOnce(&mut CookieJar) -> R) -> R {
        let mut guard = self.jars.lock().unwrap_or_else(|e| e.into_inner());
        let jar = guard.entry(strategy_name.to_string()).or_default();
        f(jar)
    }

    /// Clone the current jar for `strategy_name` (or a fresh one if absent).
    pub fn snapshot_jar(&self, strategy_name: &str) -> CookieJar {
        let guard = self.jars.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(strategy_name).cloned().unwrap_or_default()
    }
}

/// A minimal in-memory cookie jar.
///
/// Not a full RFC-6265 implementation — it stores `name=value` pairs keyed by
/// name, last-write-wins, and renders them back as a single `Cookie:` header.
/// That covers the common case of a session-cookie scraper. Domain-scoped
/// matching is the responsibility of the strategy (set `params.domain` and
/// only use the strategy for requests to that host).
#[derive(Debug, Default, Clone)]
pub struct CookieJar {
    cookies: HashMap<String, String>,
}

impl CookieJar {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or overwrite a cookie.
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.cookies.insert(name.into(), value.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.cookies.get(name).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// Render as the value of a `Cookie:` header. Keys sorted for a stable
    /// output (tests and caches appreciate determinism).
    pub fn as_header_value(&self) -> String {
        let mut pairs: Vec<(&String, &String)> = self.cookies.iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        pairs
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Parse and absorb a single `Set-Cookie` response header value.
    ///
    /// Extracts just the `name=value` pair and ignores attributes
    /// (`Path`, `Domain`, `Max-Age`, etc.). That is sufficient for the
    /// in-process session use case. Strategies wanting strict attribute
    /// handling can layer on top.
    pub fn absorb_set_cookie(&mut self, raw: &str) {
        let pair = raw.split(';').next().unwrap_or("").trim();
        if let Some((name, value)) = pair.split_once('=') {
            let name = name.trim();
            let value = value.trim();
            if !name.is_empty() {
                self.set(name, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jar_roundtrip_and_header_value() {
        let mut jar = CookieJar::new();
        jar.set("session", "abc123");
        jar.set("csrf", "xyz");
        let header = jar.as_header_value();
        // Sorted order, so csrf before session
        assert_eq!(header, "csrf=xyz; session=abc123");
    }

    #[test]
    fn jar_absorbs_set_cookie() {
        let mut jar = CookieJar::new();
        jar.absorb_set_cookie("session=abc; Path=/; HttpOnly");
        jar.absorb_set_cookie("csrf=token; Secure");
        assert_eq!(jar.get("session"), Some("abc"));
        assert_eq!(jar.get("csrf"), Some("token"));
    }

    #[test]
    fn state_with_jar_creates_on_demand() {
        let state = AuthState::new();
        state.with_jar("api", |jar| jar.set("k", "v"));
        let snap = state.snapshot_jar("api");
        assert_eq!(snap.get("k"), Some("v"));
    }

    #[test]
    fn state_per_strategy_isolated() {
        let state = AuthState::new();
        state.with_jar("a", |j| j.set("x", "1"));
        state.with_jar("b", |j| j.set("x", "2"));
        assert_eq!(state.snapshot_jar("a").get("x"), Some("1"));
        assert_eq!(state.snapshot_jar("b").get("x"), Some("2"));
    }
}
