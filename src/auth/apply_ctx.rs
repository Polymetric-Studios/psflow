use super::error::AuthError;
use super::resolver::{SecretRequest, SecretResolver};
use super::secret::SecretValue;
use super::state::AuthState;
use crate::execute::blackboard::Blackboard;
use crate::execute::Outputs;
use crate::template::TemplateResolver;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Context passed to [`super::AuthStrategy::apply`].
///
/// Bundles the lazy secret resolver, per-run mutable state, the node's
/// `inputs` and the graph blackboard (for template interpolation of
/// strategy params), and a view of the request's final body bytes and URL
/// (for body-dependent strategies like HMAC).
pub struct AuthApplyCtx<'a> {
    pub strategy_name: &'a str,
    pub secrets_map: &'a BTreeMap<String, String>,
    pub resolver: Arc<dyn SecretResolver>,
    pub state: Arc<AuthState>,
    pub inputs: &'a Outputs,
    pub blackboard: &'a Blackboard,
    pub template: Arc<dyn TemplateResolver>,
    /// The final request body bytes the handler will send, if any. HMAC
    /// signing canonicalises over these bytes. Handlers that stream bodies
    /// should materialise them before calling `apply`.
    pub body: &'a [u8],
    /// The method name the handler will send (`"GET"`, `"POST"`, ...).
    pub method: &'a str,
    /// The final request URL (post-template, post-SSRF-check).
    pub url: &'a reqwest::Url,
}

impl<'a> AuthApplyCtx<'a> {
    /// Resolve the secret bound to `role` via the strategy's `secrets` map.
    ///
    /// Translates role name → logical name → [`SecretValue`].
    pub async fn secret(&self, role: &str) -> Result<SecretValue, AuthError> {
        let logical = self
            .secrets_map
            .get(role)
            .ok_or_else(|| AuthError::MissingRole {
                name: self.strategy_name.to_string(),
                role: role.to_string(),
            })?;
        let req = SecretRequest::new(self.strategy_name.to_string(), logical.clone());
        Ok(self.resolver.resolve(&req).await?)
    }

    /// Render `template` against the node's inputs and the blackboard.
    pub fn render(&self, template: &str) -> Result<String, AuthError> {
        self.template
            .render(template, self.inputs, self.blackboard)
            .map_err(|e| AuthError::Template {
                name: self.strategy_name.to_string(),
                message: e.to_string(),
            })
    }
}
