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
///
/// ## CSRF cookie-to-header echo
///
/// Some frameworks (Laravel) require a CSRF token that lives in a cookie to be
/// echoed back in a request header. Set both `csrf_cookie` and `csrf_header` to
/// copy the named cookie's value into the named header on every request:
/// `{ "csrf_cookie": "XSRF-TOKEN", "csrf_header": "x-xsrf-token" }`. The value is
/// URL-decoded first (Laravel stores `XSRF-TOKEN` percent-encoded); set
/// `csrf_url_decode: false` to echo verbatim. The two params must be set together.
pub struct CookieJarStrategy {
    /// Configured domain restriction, lowercased. `None` → accept any host.
    domain: Option<String>,
    /// Cookie name to echo into a request header (CSRF). Paired with `csrf_header`.
    csrf_cookie: Option<String>,
    /// Header name that carries the echoed cookie value. Paired with `csrf_cookie`.
    csrf_header: Option<String>,
    /// URL-decode the cookie value before echoing (default true).
    csrf_url_decode: bool,
}

impl CookieJarStrategy {
    pub fn from_decl(decl: &AuthStrategyDecl) -> Result<Arc<dyn AuthStrategy>, AuthError> {
        let domain = decl
            .params
            .get("domain")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase());

        let str_param = |key: &str| {
            decl.params
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        };
        let csrf_cookie = str_param("csrf_cookie");
        let csrf_header = str_param("csrf_header");
        if csrf_cookie.is_some() != csrf_header.is_some() {
            return Err(AuthError::Config {
                name: COOKIE_JAR_TYPE.to_string(),
                message: "csrf_cookie and csrf_header must be set together".to_string(),
            });
        }
        let csrf_url_decode = decl
            .params
            .get("csrf_url_decode")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        Ok(Arc::new(Self {
            domain,
            csrf_cookie,
            csrf_header,
            csrf_url_decode,
        }))
    }

    /// Compute the `(header_name, header_value)` to add for the CSRF echo, if
    /// configured and the source cookie is present in `jar`.
    fn csrf_pair(&self, jar: &crate::auth::state::CookieJar) -> Option<(String, String)> {
        let cookie_name = self.csrf_cookie.as_deref()?;
        let header_name = self.csrf_header.as_deref()?;
        let raw = jar.get(cookie_name)?;
        let value = if self.csrf_url_decode {
            percent_decode(raw)
        } else {
            raw.to_string()
        };
        Some((header_name.to_string(), value))
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

        let jar = ctx.state.snapshot_jar(ctx.strategy_name);
        let mut builder = builder;
        let header = jar.as_header_value();
        if !header.is_empty() {
            builder = builder.header(reqwest::header::COOKIE, header);
        }
        if let Some((name, value)) = self.csrf_pair(&jar) {
            builder = builder.header(name, value);
        }
        Ok(builder)
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

        let jar = ctx.state.snapshot_jar(ctx.strategy_name);
        let header = jar.as_header_value();
        if !header.is_empty() {
            let value = http::HeaderValue::try_from(header).map_err(|e| AuthError::Apply {
                name: ctx.strategy_name.to_string(),
                message: format!("invalid cookie header value: {e}"),
            })?;
            request.headers_mut().insert(http::header::COOKIE, value);
        }
        if let Some((name, value)) = self.csrf_pair(&jar) {
            let hname = http::header::HeaderName::try_from(name.as_str()).map_err(|e| {
                AuthError::Apply {
                    name: ctx.strategy_name.to_string(),
                    message: format!("invalid csrf header name '{name}': {e}"),
                }
            })?;
            let hval = http::HeaderValue::try_from(value).map_err(|e| AuthError::Apply {
                name: ctx.strategy_name.to_string(),
                message: format!("invalid csrf header value: {e}"),
            })?;
            request.headers_mut().insert(hname, hval);
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

/// Minimal percent-decoder for cookie values: decodes `%XX` escapes and leaves
/// malformed/partial escapes untouched. Sufficient for Laravel's URL-encoded
/// `XSRF-TOKEN` cookie (where `=` is stored as `%3D`). Not a general URL decoder
/// (does not treat `+` as space — cookie values are not form-encoded).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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

    // ── percent_decode + CSRF echo ───────────────────────────────────────────

    #[test]
    fn percent_decode_decodes_and_passes_through() {
        assert_eq!(percent_decode("eyJpdiI%3D%3D"), "eyJpdiI==");
        assert_eq!(percent_decode("a%2Bb%2Fc"), "a+b/c");
        assert_eq!(percent_decode("nothing-encoded"), "nothing-encoded");
        // Malformed / trailing escapes are left untouched.
        assert_eq!(percent_decode("ab%"), "ab%");
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    #[test]
    fn csrf_params_must_be_paired() {
        let decl = AuthStrategyDecl::new(COOKIE_JAR_TYPE)
            .with_params(serde_json::json!({"csrf_cookie": "XSRF-TOKEN"}));
        match CookieJarStrategy::from_decl(&decl) {
            Err(AuthError::Config { .. }) => {}
            Err(other) => panic!("expected Config error, got {other:?}"),
            Ok(_) => panic!("expected an error, got Ok"),
        }
    }

    #[tokio::test]
    async fn csrf_echo_adds_url_decoded_header() {
        let state = Arc::new(AuthState::new());
        // Laravel stores XSRF-TOKEN percent-encoded.
        state.with_jar("jar", |j| j.set("XSRF-TOKEN", "tok%3D%3D"));
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("https://api.example.com/x").unwrap();

        let strategy = CookieJarStrategy::from_decl(
            &AuthStrategyDecl::new(COOKIE_JAR_TYPE).with_params(serde_json::json!({
                "csrf_cookie": "XSRF-TOKEN",
                "csrf_header": "x-xsrf-token"
            })),
        )
        .unwrap();

        let ctx = make_ctx(state, &secrets, &inputs, &bb, &url);
        let client = reqwest::Client::new();
        let req = strategy
            .apply(&ctx, client.get(url.clone()))
            .await
            .unwrap()
            .build()
            .unwrap();

        assert_eq!(
            req.headers().get("x-xsrf-token").unwrap().to_str().unwrap(),
            "tok=="
        );
        // The cookie itself still rides along verbatim (encoded).
        assert!(req
            .headers()
            .get("cookie")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("XSRF-TOKEN=tok%3D%3D"));
    }

    #[tokio::test]
    async fn csrf_echo_verbatim_when_decode_disabled() {
        let state = Arc::new(AuthState::new());
        state.with_jar("jar", |j| j.set("XSRF-TOKEN", "tok%3D%3D"));
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("https://api.example.com/x").unwrap();

        let strategy = CookieJarStrategy::from_decl(
            &AuthStrategyDecl::new(COOKIE_JAR_TYPE).with_params(serde_json::json!({
                "csrf_cookie": "XSRF-TOKEN",
                "csrf_header": "x-xsrf-token",
                "csrf_url_decode": false
            })),
        )
        .unwrap();

        let ctx = make_ctx(state, &secrets, &inputs, &bb, &url);
        let client = reqwest::Client::new();
        let req = strategy
            .apply(&ctx, client.get(url.clone()))
            .await
            .unwrap()
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get("x-xsrf-token").unwrap().to_str().unwrap(),
            "tok%3D%3D"
        );
    }

    #[tokio::test]
    async fn csrf_absent_when_cookie_missing() {
        let state = Arc::new(AuthState::new());
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("https://api.example.com/x").unwrap();

        let strategy = CookieJarStrategy::from_decl(
            &AuthStrategyDecl::new(COOKIE_JAR_TYPE).with_params(serde_json::json!({
                "csrf_cookie": "XSRF-TOKEN",
                "csrf_header": "x-xsrf-token"
            })),
        )
        .unwrap();

        let ctx = make_ctx(state, &secrets, &inputs, &bb, &url);
        let client = reqwest::Client::new();
        let req = strategy
            .apply(&ctx, client.get(url.clone()))
            .await
            .unwrap()
            .build()
            .unwrap();
        assert!(req.headers().get("x-xsrf-token").is_none());
    }

    #[tokio::test]
    async fn csrf_echo_on_ws_handshake() {
        let state = Arc::new(AuthState::new());
        state.with_jar("jar", |j| j.set("XSRF-TOKEN", "tok%3D"));
        let secrets = BTreeMap::new();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let url = reqwest::Url::parse("https://api.example.com/x").unwrap();

        let strategy = CookieJarStrategy::from_decl(
            &AuthStrategyDecl::new(COOKIE_JAR_TYPE).with_params(serde_json::json!({
                "csrf_cookie": "XSRF-TOKEN",
                "csrf_header": "x-xsrf-token"
            })),
        )
        .unwrap();

        let ctx = make_ctx(state, &secrets, &inputs, &bb, &url);
        let request = http::Request::builder()
            .uri("wss://api.example.com/socket")
            .body(())
            .unwrap();
        let out = strategy.apply_ws_request(&ctx, request).await.unwrap();
        assert_eq!(
            out.headers().get("x-xsrf-token").unwrap().to_str().unwrap(),
            "tok="
        );
    }
}
