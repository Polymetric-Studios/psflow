use crate::auth::apply_ctx::AuthApplyCtx;
use crate::auth::decl::AuthStrategyDecl;
use crate::auth::error::AuthError;
use crate::auth::strategy::AuthStrategy;
use async_trait::async_trait;
use reqwest::RequestBuilder;
use std::sync::Arc;

pub const BEARER_TYPE: &str = "bearer";

const DEFAULT_HEADER: &str = "Authorization";
const DEFAULT_SCHEME: &str = "Bearer";
const TOKEN_ROLE: &str = "token";

/// Bearer-token auth. Renders `<header>: <scheme> <token>` where `token`
/// is resolved from the strategy's `secrets.token` logical name.
///
/// Params (all optional): `{ "header": "Authorization", "scheme": "Bearer" }`.
pub struct BearerStrategy {
    header: String,
    scheme: String,
}

impl BearerStrategy {
    pub fn from_decl(decl: &AuthStrategyDecl) -> Result<Arc<dyn AuthStrategy>, AuthError> {
        let (header, scheme) = match &decl.params {
            serde_json::Value::Null => (DEFAULT_HEADER.to_string(), DEFAULT_SCHEME.to_string()),
            serde_json::Value::Object(obj) => {
                let header = obj
                    .get("header")
                    .and_then(|v| v.as_str())
                    .unwrap_or(DEFAULT_HEADER)
                    .to_string();
                let scheme = obj
                    .get("scheme")
                    .and_then(|v| v.as_str())
                    .unwrap_or(DEFAULT_SCHEME)
                    .to_string();
                (header, scheme)
            }
            _ => {
                return Err(AuthError::Config {
                    name: BEARER_TYPE.to_string(),
                    message: "params must be an object or absent".to_string(),
                });
            }
        };
        Ok(Arc::new(Self { header, scheme }))
    }
}

impl BearerStrategy {
    async fn header_value(&self, ctx: &AuthApplyCtx<'_>) -> Result<(String, String), AuthError> {
        let token = ctx.secret(TOKEN_ROLE).await?;
        let token_str = token.reveal_str().ok_or_else(|| AuthError::Apply {
            name: ctx.strategy_name.to_string(),
            message: "bearer token secret is not valid UTF-8".to_string(),
        })?;
        let header_value = if self.scheme.is_empty() {
            token_str.to_string()
        } else {
            format!("{} {}", self.scheme, token_str)
        };
        Ok((self.header.clone(), header_value))
    }
}

#[async_trait]
impl AuthStrategy for BearerStrategy {
    fn type_name(&self) -> &'static str {
        BEARER_TYPE
    }

    fn required_roles(&self) -> &'static [&'static str] {
        &[TOKEN_ROLE]
    }

    fn supports_ws(&self) -> bool {
        true
    }

    async fn apply(
        &self,
        ctx: &AuthApplyCtx<'_>,
        builder: RequestBuilder,
    ) -> Result<RequestBuilder, AuthError> {
        let (name, value) = self.header_value(ctx).await?;
        Ok(builder.header(name.as_str(), value))
    }

    async fn apply_ws_request(
        &self,
        ctx: &AuthApplyCtx<'_>,
        mut request: http::Request<()>,
    ) -> Result<http::Request<()>, AuthError> {
        let (name, value) = self.header_value(ctx).await?;
        let header_name =
            http::HeaderName::try_from(name.as_str()).map_err(|e| AuthError::Apply {
                name: ctx.strategy_name.to_string(),
                message: format!("invalid bearer header name '{name}': {e}"),
            })?;
        let header_value = http::HeaderValue::try_from(value).map_err(|e| AuthError::Apply {
            name: ctx.strategy_name.to_string(),
            message: format!("invalid bearer header value: {e}"),
        })?;
        request.headers_mut().insert(header_name, header_value);
        Ok(request)
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
    use std::collections::BTreeMap;

    #[test]
    fn required_roles_is_token() {
        let s = BearerStrategy::from_decl(&AuthStrategyDecl::new(BEARER_TYPE)).unwrap();
        assert_eq!(s.required_roles(), &["token"]);
    }

    #[tokio::test]
    async fn apply_injects_authorization_header() {
        let resolver = Arc::new(StaticSecretResolver::new());
        resolver.insert("b", "token", "secret123");
        let state = Arc::new(AuthState::new());

        let strategy = BearerStrategy::from_decl(&AuthStrategyDecl::new(BEARER_TYPE)).unwrap();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let mut secrets = BTreeMap::new();
        secrets.insert("token".into(), "my_key".into());
        // The resolver keys under "b:token" via insert above, but request uses
        // strategy_name="b" + logical_name="my_key". Adjust:
        resolver.insert("b", "my_key", "secret123");

        let url = reqwest::Url::parse("http://example.com").unwrap();
        let ctx = AuthApplyCtx {
            strategy_name: "b",
            secrets_map: &secrets,
            resolver: resolver.clone() as Arc<dyn SecretResolver>,
            state,
            inputs: &inputs,
            blackboard: &bb,
            template: default_resolver(),
            body: &[],
            method: "GET",
            url: &url,
        };

        let client = reqwest::Client::new();
        let built = strategy.apply(&ctx, client.get(url.clone())).await.unwrap();
        let req = built.build().unwrap();
        assert_eq!(
            req.headers().get("authorization").unwrap(),
            "Bearer secret123"
        );
    }

    #[tokio::test]
    async fn custom_header_and_scheme() {
        let resolver = Arc::new(StaticSecretResolver::new());
        resolver.insert("b", "my_key", "tok");
        let state = Arc::new(AuthState::new());

        let decl = AuthStrategyDecl::new(BEARER_TYPE).with_params(serde_json::json!({
            "header": "X-Auth",
            "scheme": "Token",
        }));
        let strategy = BearerStrategy::from_decl(&decl).unwrap();

        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let mut secrets = BTreeMap::new();
        secrets.insert("token".into(), "my_key".into());
        let url = reqwest::Url::parse("http://example.com").unwrap();
        let ctx = AuthApplyCtx {
            strategy_name: "b",
            secrets_map: &secrets,
            resolver: resolver as Arc<dyn SecretResolver>,
            state,
            inputs: &inputs,
            blackboard: &bb,
            template: default_resolver(),
            body: &[],
            method: "GET",
            url: &url,
        };

        let client = reqwest::Client::new();
        let built = strategy.apply(&ctx, client.get(url.clone())).await.unwrap();
        let req = built.build().unwrap();
        assert_eq!(req.headers().get("x-auth").unwrap(), "Token tok");
    }

    #[tokio::test]
    async fn missing_role_errors_on_apply() {
        let resolver = Arc::new(StaticSecretResolver::new());
        let state = Arc::new(AuthState::new());
        let strategy = BearerStrategy::from_decl(&AuthStrategyDecl::new(BEARER_TYPE)).unwrap();
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let secrets = BTreeMap::new(); // empty — missing role
        let url = reqwest::Url::parse("http://example.com").unwrap();
        let ctx = AuthApplyCtx {
            strategy_name: "b",
            secrets_map: &secrets,
            resolver: resolver as Arc<dyn SecretResolver>,
            state,
            inputs: &inputs,
            blackboard: &bb,
            template: default_resolver(),
            body: &[],
            method: "GET",
            url: &url,
        };
        let client = reqwest::Client::new();
        let err = strategy
            .apply(&ctx, client.get(url.clone()))
            .await
            .unwrap_err();
        assert!(matches!(err, AuthError::MissingRole { .. }));
    }
}
