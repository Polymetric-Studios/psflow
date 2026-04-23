//! Serializable declaration of a graph-scoped auth strategy.
//!
//! Lives in the always-available graph slice so `GraphMetadata` can embed
//! declarations without depending on the runtime-only `auth` module. The
//! runtime's `auth::AuthStrategyDecl` is a re-export of this type.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Declaration of a single graph-scoped auth strategy, stored in
/// [`crate::graph::metadata::GraphMetadata::auth`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthStrategyDecl {
    /// Discriminator selecting a factory registered in
    /// `auth::AuthStrategyRegistry`. Built-ins: `"static_header"`,
    /// `"bearer"`, `"hmac"`, `"cookie_jar"`.
    #[serde(rename = "type")]
    pub type_: String,
    /// Strategy-specific configuration. Shape validated by the factory at
    /// graph load time. Values may contain `{inputs.x}` / `{ctx.x}`
    /// template placeholders rendered at `apply` time.
    #[serde(default)]
    pub params: serde_json::Value,
    /// Role-name → logical-name map. Role names are what the strategy
    /// asks for (e.g. `token` for bearer); logical names are what the
    /// host-side `SecretResolver` understands.
    #[serde(default)]
    pub secrets: BTreeMap<String, String>,
}

impl AuthStrategyDecl {
    pub fn new(type_: impl Into<String>) -> Self {
        Self {
            type_: type_.into(),
            params: serde_json::Value::Null,
            secrets: BTreeMap::new(),
        }
    }

    pub fn with_params(mut self, params: serde_json::Value) -> Self {
        self.params = params;
        self
    }

    pub fn with_secret(mut self, role: impl Into<String>, logical: impl Into<String>) -> Self {
        self.secrets.insert(role.into(), logical.into());
        self
    }
}
