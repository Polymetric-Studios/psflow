use crate::auth::apply_ctx::AuthApplyCtx;
use crate::auth::decl::AuthStrategyDecl;
use crate::auth::error::AuthError;
use crate::auth::strategy::AuthStrategy;
use async_trait::async_trait;
use reqwest::RequestBuilder;
use std::sync::Arc;

pub const STATIC_HEADER_TYPE: &str = "static_header";

/// Inject a single fixed header.
///
/// Params: `{ "name": "X-Api-Key", "value": "abc" }`.
/// `value` may use `{inputs.x}` / `{ctx.x}` interpolation and can also
/// reference a resolved secret via the `{inputs.x}` mechanism applied to
/// node inputs — for a genuine "secret in a header" case, use `bearer`
/// or build a custom strategy rather than leaking the secret through
/// node inputs.
pub struct StaticHeaderStrategy {
    name_template: String,
    value_template: String,
}

impl StaticHeaderStrategy {
    pub fn from_decl(decl: &AuthStrategyDecl) -> Result<Arc<dyn AuthStrategy>, AuthError> {
        let obj = decl.params.as_object().ok_or_else(|| AuthError::Config {
            name: STATIC_HEADER_TYPE.to_string(),
            message: "params must be an object".to_string(),
        })?;
        let name = obj
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AuthError::Config {
                name: STATIC_HEADER_TYPE.to_string(),
                message: "params.name (string) required".to_string(),
            })?
            .to_string();
        let value = obj
            .get("value")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AuthError::Config {
                name: STATIC_HEADER_TYPE.to_string(),
                message: "params.value (string) required".to_string(),
            })?
            .to_string();
        Ok(Arc::new(Self {
            name_template: name,
            value_template: value,
        }))
    }
}

impl StaticHeaderStrategy {
    fn rendered(&self, ctx: &AuthApplyCtx<'_>) -> Result<(String, String), AuthError> {
        let header_name = ctx.render(&self.name_template)?;
        let header_value = ctx.render(&self.value_template)?;
        Ok((header_name, header_value))
    }
}

#[async_trait]
impl AuthStrategy for StaticHeaderStrategy {
    fn type_name(&self) -> &'static str {
        STATIC_HEADER_TYPE
    }

    fn supports_ws(&self) -> bool {
        true
    }

    async fn apply(
        &self,
        ctx: &AuthApplyCtx<'_>,
        builder: RequestBuilder,
    ) -> Result<RequestBuilder, AuthError> {
        let (name, value) = self.rendered(ctx)?;
        Ok(builder.header(name, value))
    }

    async fn apply_ws_request(
        &self,
        ctx: &AuthApplyCtx<'_>,
        mut request: http::Request<()>,
    ) -> Result<http::Request<()>, AuthError> {
        let (name, value) = self.rendered(ctx)?;
        let header_name =
            http::HeaderName::try_from(name.as_str()).map_err(|e| AuthError::Apply {
                name: ctx.strategy_name.to_string(),
                message: format!("invalid static header name '{name}': {e}"),
            })?;
        let header_value = http::HeaderValue::try_from(value).map_err(|e| AuthError::Apply {
            name: ctx.strategy_name.to_string(),
            message: format!("invalid static header value: {e}"),
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

    fn test_decl(params: serde_json::Value) -> AuthStrategyDecl {
        AuthStrategyDecl::new(STATIC_HEADER_TYPE).with_params(params)
    }

    #[test]
    fn from_decl_requires_object_params() {
        let decl = AuthStrategyDecl::new(STATIC_HEADER_TYPE);
        assert!(StaticHeaderStrategy::from_decl(&decl).is_err());
    }

    #[test]
    fn from_decl_requires_name_and_value() {
        let decl = test_decl(serde_json::json!({ "name": "X" }));
        assert!(StaticHeaderStrategy::from_decl(&decl).is_err());
    }

    #[tokio::test]
    async fn apply_injects_header() {
        let decl = test_decl(serde_json::json!({
            "name": "X-Api-Key",
            "value": "abc-123",
        }));
        let strategy = StaticHeaderStrategy::from_decl(&decl).unwrap();

        let resolver: Arc<dyn SecretResolver> = Arc::new(StaticSecretResolver::new());
        let state = Arc::new(AuthState::new());
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        let secrets = BTreeMap::new();
        let url = reqwest::Url::parse("http://example.com").unwrap();
        let ctx = AuthApplyCtx {
            strategy_name: "h",
            secrets_map: &secrets,
            resolver,
            state,
            inputs: &inputs,
            blackboard: &bb,
            template: default_resolver(),
            body: &[],
            method: "GET",
            url: &url,
        };

        let client = reqwest::Client::new();
        let builder = client.get(url.clone());
        let built = strategy.apply(&ctx, builder).await.unwrap();
        let req = built.build().unwrap();
        assert_eq!(req.headers().get("x-api-key").unwrap(), "abc-123");
    }
}
