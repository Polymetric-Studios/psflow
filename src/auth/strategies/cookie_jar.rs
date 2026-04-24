use crate::auth::apply_ctx::AuthApplyCtx;
use crate::auth::decl::AuthStrategyDecl;
use crate::auth::error::AuthError;
use crate::auth::strategy::AuthStrategy;
use async_trait::async_trait;
use reqwest::header::{HeaderMap, SET_COOKIE};
use reqwest::RequestBuilder;
use std::sync::Arc;

pub const COOKIE_JAR_TYPE: &str = "cookie_jar";

/// Per-run cookie jar strategy. Sends the current jar as a `Cookie:` header
/// and absorbs `Set-Cookie` from responses into the run-scoped jar held on
/// [`crate::auth::AuthState`].
///
/// Params (all optional): `{ "domain": "example.com" }`. When `domain` is set,
/// the strategy enforces suffix-match semantics at apply time and filters
/// Set-Cookie responses by domain. See [`domain_matches`] for match rules.
pub struct CookieJarStrategy {
    /// Configured domain restriction, lowercased. `None` → accept any host.
    domain: Option<String>,
}

impl CookieJarStrategy {
    pub fn from_decl(decl: &AuthStrategyDecl) -> Result<Arc<dyn AuthStrategy>, AuthError> {
        let domain = decl
            .params
            .get("domain")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase());
        Ok(Arc::new(Self { domain }))
    }
}

/// Returns `true` if `host` is permitted by `configured_domain`.
///
/// Rules:
/// - If `configured_domain` is an IP address literal, the match is exact
///   (case-insensitive, port stripped from `host`).
/// - Otherwise, `host` matches if it equals `configured_domain` OR ends with
///   `.<configured_domain>` (standard cookie suffix semantics).
/// - Comparison is case-insensitive; port is stripped from `host` before
///   comparing.
pub fn domain_matches(configured_domain: &str, host: &str) -> bool {
    // Strip port from the host segment.
    let host_bare = host
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host)
        .to_lowercase();

    let domain = configured_domain.to_lowercase();

    // Exact match always works (covers IPs and plain hostnames).
    if host_bare == domain {
        return true;
    }

    // IP literals: no subdomain logic — exact match only.
    if domain.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }

    // Suffix match: host ends with ".<domain>".
    host_bare.ends_with(&format!(".{domain}"))
}

#[async_trait]
impl AuthStrategy for CookieJarStrategy {
    fn type_name(&self) -> &'static str {
        COOKIE_JAR_TYPE
    }

    fn supports_ws(&self) -> bool {
        true
    }

    async fn apply(
        &self,
        ctx: &AuthApplyCtx<'_>,
        builder: RequestBuilder,
    ) -> Result<RequestBuilder, AuthError> {
        if let Some(ref configured_domain) = self.domain {
            let host = ctx.url.host_str().unwrap_or("");
            if !domain_matches(configured_domain, host) {
                return Err(AuthError::Apply {
                    name: ctx.strategy_name.to_string(),
                    message: format!(
                        "cookie_jar domain '{configured_domain}' does not match request host '{host}'"
                    ),
                });
            }
        }

        let header = ctx.state.snapshot_jar(ctx.strategy_name).as_header_value();
        if header.is_empty() {
            Ok(builder)
        } else {
            Ok(builder.header(reqwest::header::COOKIE, header))
        }
    }

    async fn apply_ws_request(
        &self,
        ctx: &AuthApplyCtx<'_>,
        mut request: http::Request<()>,
    ) -> Result<http::Request<()>, AuthError> {
        if let Some(ref configured_domain) = self.domain {
            let host = ctx.url.host_str().unwrap_or("");
            if !domain_matches(configured_domain, host) {
                return Err(AuthError::Apply {
                    name: ctx.strategy_name.to_string(),
                    message: format!(
                        "cookie_jar domain '{configured_domain}' does not match request host '{host}'"
                    ),
                });
            }
        }

        let header = ctx.state.snapshot_jar(ctx.strategy_name).as_header_value();
        if !header.is_empty() {
            let value = http::HeaderValue::try_from(header).map_err(|e| AuthError::Apply {
                name: ctx.strategy_name.to_string(),
                message: format!("invalid cookie header value: {e}"),
            })?;
            request.headers_mut().insert(http::header::COOKIE, value);
        }
        Ok(request)
    }

    async fn observe_response(
        &self,
        ctx: &AuthApplyCtx<'_>,
        headers: &HeaderMap,
    ) -> Result<(), AuthError> {
        let name = ctx.strategy_name;
        let request_host = ctx.url.host_str().unwrap_or("").to_lowercase();

        ctx.state.with_jar(name, |jar| {
            for value in headers.get_all(SET_COOKIE).iter() {
                if let Ok(s) = value.to_str() {
                    if let Some(ref configured_domain) = self.domain {
                        // If the Set-Cookie header has a Domain= attribute, that
                        // domain must match the configured domain.  If there is no
                        // Domain= attribute, the cookie is implicitly scoped to the
                        // response host — which must also match.
                        let cookie_domain = parse_set_cookie_domain(s);
                        let effective_host = cookie_domain.as_deref().unwrap_or(&request_host);
                        if !domain_matches(configured_domain, effective_host) {
                            continue;
                        }
                    }
                    jar.absorb_set_cookie(s);
                }
            }
        });
        Ok(())
    }
}

/// Extract the `Domain=` attribute value from a raw Set-Cookie string, if
/// present. Returns `None` when the attribute is absent.
fn parse_set_cookie_domain(set_cookie: &str) -> Option<String> {
    for part in set_cookie.split(';').skip(1) {
        let part = part.trim();
        if let Some(val) = part
            .strip_prefix("Domain=")
            .or_else(|| part.strip_prefix("domain="))
        {
            let trimmed = val.trim().trim_start_matches('.');
            if !trimmed.is_empty() {
                return Some(trimmed.to_lowercase());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::resolver::{SecretResolver, StaticSecretResolver};
    use crate::auth::state::AuthState;
    use crate::execute::blackboard::Blackboard;
    use crate::execute::Outputs;
    use crate::template::default_resolver;
    use reqwest::header::HeaderValue;
    use std::collections::BTreeMap;

    fn make_ctx<'a>(
        state: Arc<AuthState>,
        secrets: &'a BTreeMap<String, String>,
        inputs: &'a Outputs,
        bb: &'a Blackboard,
        url: &'a reqwest::Url,
    ) -> AuthApplyCtx<'a> {
        AuthApplyCtx {
            strategy_name: "jar",
            secrets_map: secrets,
            resolver: Arc::new(StaticSecretResolver::new()) as Arc<dyn SecretResolver>,
            state,
            inputs,
            blackboard: bb,
            template: default_resolver(),
            body: &[],
            method: "GET",
            url,
        }
    }

    // ── domain_matches unit tests ─────────────────────────────────────────────

    #[test]
    fn exact_match() {
        assert!(domain_matches("example.com", "example.com"));
    }

    #[test]
    fn subdomain_match() {
        assert!(domain_matches("example.com", "api.example.com"));
        assert!(domain_matches("example.com", "v2.api.example.com"));
    }

    #[test]
    fn no_match_different_domain() {
        assert!(!domain_matches("example.com", "other.com"));
    }

    #[test]
    fn no_partial_suffix_match() {
        // "notexample.com" must NOT match "example.com".
        assert!(!domain_matches("example.com", "notexample.com"));
    }

    #[test]
    fn case_insensitive() {
        assert!(domain_matches("Example.COM", "API.example.com"));
    }

    #[test]
    fn port_is_ignored() {
        assert!(domain_matches("example.com", "example.com:8080"));
        assert!(domain_matches("example.com", "api.example.com:443"));
    }

    #[test]
    fn ip_exact_match() {
        assert!(domain_matches("127.0.0.1", "127.0.0.1"));
    }

    #[test]
    fn ip_no_subdomain_match() {
        // Sub-labels of IP literals are not real hosts — no suffix logic.
        assert!(!domain_matches("127.0.0.1", "sub.127.0.0.1"));
    }

    #[test]
    fn ip_port_stripped() {
        assert!(domain_matches("127.0.0.1", "127.0.0.1:9000"));
    }

    // ── parse_set_cookie_domain unit tests ───────────────────────────────────

    #[test]
    fn parses_domain_attribute() {
        assert_eq!(
            parse_set_cookie_domain("foo=bar; Domain=example.com; Path=/"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn parses_leading_dot_domain() {
        assert_eq!(
            parse_set_cookie_domain("foo=bar; Domain=.example.com"),
            Some("example.com".to_string())
        );
    }

    #[test]
    fn no_domain_attribute_returns_none() {
        assert_eq!(parse_set_cookie_domain("foo=bar; Path=/; HttpOnly"), None);
    }

    // ── strategy integration tests ───────────────────────────────────────────

    #[tokio::test]
    async fn empty_jar_skips_header() {
        let state = Arc::new(AuthState::new());
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("http://example.com").unwrap();
        let ctx = make_ctx(state, &secrets, &inputs, &bb, &url);
        let strategy =
            CookieJarStrategy::from_decl(&AuthStrategyDecl::new(COOKIE_JAR_TYPE)).unwrap();
        let client = reqwest::Client::new();
        let built = strategy.apply(&ctx, client.get(url.clone())).await.unwrap();
        let req = built.build().unwrap();
        assert!(req.headers().get("cookie").is_none());
    }

    #[tokio::test]
    async fn observe_then_apply_round_trip() {
        let state = Arc::new(AuthState::new());
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("http://example.com").unwrap();

        let strategy =
            CookieJarStrategy::from_decl(&AuthStrategyDecl::new(COOKIE_JAR_TYPE)).unwrap();

        // Simulate a response with Set-Cookie.
        let mut resp_headers = HeaderMap::new();
        resp_headers.append(
            SET_COOKIE,
            HeaderValue::from_static("session=abc; Path=/; HttpOnly"),
        );
        resp_headers.append(SET_COOKIE, HeaderValue::from_static("csrf=xyz"));

        {
            let ctx = make_ctx(state.clone(), &secrets, &inputs, &bb, &url);
            strategy
                .observe_response(&ctx, &resp_headers)
                .await
                .unwrap();
        }

        // Next request should carry the jar.
        let ctx = make_ctx(state, &secrets, &inputs, &bb, &url);
        let client = reqwest::Client::new();
        let built = strategy.apply(&ctx, client.get(url.clone())).await.unwrap();
        let req = built.build().unwrap();
        let cookie = req.headers().get("cookie").unwrap().to_str().unwrap();
        assert!(cookie.contains("session=abc"));
        assert!(cookie.contains("csrf=xyz"));
    }

    #[tokio::test]
    async fn domain_exact_match_allows_apply() {
        let state = Arc::new(AuthState::new());
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("https://api.example.com/users/1").unwrap();

        let strategy = CookieJarStrategy::from_decl(
            &AuthStrategyDecl::new(COOKIE_JAR_TYPE)
                .with_params(serde_json::json!({"domain": "api.example.com"})),
        )
        .unwrap();

        let ctx = make_ctx(state, &secrets, &inputs, &bb, &url);
        let client = reqwest::Client::new();
        // Should not error — host matches configured domain exactly.
        let result = strategy.apply(&ctx, client.get(url.clone())).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn domain_subdomain_match_allows_apply() {
        let state = Arc::new(AuthState::new());
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("https://v2.api.example.com/endpoint").unwrap();

        let strategy = CookieJarStrategy::from_decl(
            &AuthStrategyDecl::new(COOKIE_JAR_TYPE)
                .with_params(serde_json::json!({"domain": "example.com"})),
        )
        .unwrap();

        let ctx = make_ctx(state, &secrets, &inputs, &bb, &url);
        let client = reqwest::Client::new();
        let result = strategy.apply(&ctx, client.get(url.clone())).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn domain_mismatch_fails_apply() {
        let state = Arc::new(AuthState::new());
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("https://other.com/api").unwrap();

        let strategy = CookieJarStrategy::from_decl(
            &AuthStrategyDecl::new(COOKIE_JAR_TYPE)
                .with_params(serde_json::json!({"domain": "example.com"})),
        )
        .unwrap();

        let ctx = make_ctx(state, &secrets, &inputs, &bb, &url);
        let client = reqwest::Client::new();
        let err = strategy
            .apply(&ctx, client.get(url.clone()))
            .await
            .unwrap_err();
        match err {
            AuthError::Apply { ref message, .. } => {
                assert!(message.contains("example.com"), "error: {message}");
                assert!(message.contains("other.com"), "error: {message}");
            }
            other => panic!("expected Apply error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn ip_literal_exact_match_apply() {
        let state = Arc::new(AuthState::new());
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("http://127.0.0.1:8080/health").unwrap();

        let strategy = CookieJarStrategy::from_decl(
            &AuthStrategyDecl::new(COOKIE_JAR_TYPE)
                .with_params(serde_json::json!({"domain": "127.0.0.1"})),
        )
        .unwrap();

        let ctx = make_ctx(state, &secrets, &inputs, &bb, &url);
        let client = reqwest::Client::new();
        let result = strategy.apply(&ctx, client.get(url.clone())).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn observe_ignores_cookie_with_mismatched_domain_attribute() {
        let state = Arc::new(AuthState::new());
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("https://api.example.com/login").unwrap();

        let strategy = CookieJarStrategy::from_decl(
            &AuthStrategyDecl::new(COOKIE_JAR_TYPE)
                .with_params(serde_json::json!({"domain": "example.com"})),
        )
        .unwrap();

        // Server sends a cookie scoped to a completely different domain.
        let mut resp_headers = HeaderMap::new();
        resp_headers.append(
            SET_COOKIE,
            HeaderValue::from_static("foo=bar; Domain=other.com; Path=/"),
        );

        let ctx = make_ctx(state.clone(), &secrets, &inputs, &bb, &url);
        strategy
            .observe_response(&ctx, &resp_headers)
            .await
            .unwrap();

        // Jar must be empty — the cookie was not recorded.
        let jar = state.snapshot_jar("jar");
        let header_val = jar.as_header_value();
        assert!(
            header_val.is_empty(),
            "stray-domain cookie must not be recorded; got: {header_val}"
        );
    }

    #[tokio::test]
    async fn absent_domain_accepts_any_host() {
        // Backward-compat: no domain param → any host allowed.
        let state = Arc::new(AuthState::new());
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();

        let strategy =
            CookieJarStrategy::from_decl(&AuthStrategyDecl::new(COOKIE_JAR_TYPE)).unwrap();

        for host in &[
            "https://example.com/a",
            "https://other.org/b",
            "http://127.0.0.1/c",
        ] {
            let url = reqwest::Url::parse(host).unwrap();
            let ctx = make_ctx(state.clone(), &secrets, &inputs, &bb, &url);
            let client = reqwest::Client::new();
            let result = strategy.apply(&ctx, client.get(url.clone())).await;
            assert!(result.is_ok(), "host={host} should be accepted");
        }
    }
}
