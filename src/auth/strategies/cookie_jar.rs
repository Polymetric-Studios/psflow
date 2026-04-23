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
/// Params (all optional): `{ "domain": "example.com" }`. The `domain` param
/// is informational — matching is the caller's responsibility via using
/// this strategy only on handlers targeting that host.
pub struct CookieJarStrategy;

impl CookieJarStrategy {
    pub fn from_decl(_decl: &AuthStrategyDecl) -> Result<Arc<dyn AuthStrategy>, AuthError> {
        Ok(Arc::new(Self))
    }
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
        ctx.state.with_jar(name, |jar| {
            for value in headers.get_all(SET_COOKIE).iter() {
                if let Ok(s) = value.to_str() {
                    jar.absorb_set_cookie(s);
                }
            }
        });
        Ok(())
    }
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
}
