//! Graph-scoped authentication strategy layer.
//!
//! See `ergon/active-documents/20260423-084729-psflow-auth-layer-design.md`
//! for the design rationale. The short story:
//!
//! - Graphs declare **named strategies** at graph scope via
//!   [`crate::graph::metadata::GraphMetadata::auth`].
//! - Network handlers reference a strategy by name in their `config.auth`.
//! - At node execution, the handler looks up the strategy in an
//!   [`AuthStrategyRegistry`], obtains secrets through a host-provided
//!   [`SecretResolver`], and calls [`AuthStrategy::apply`] on the
//!   `reqwest::RequestBuilder`.
//! - Post-response hooks (cookie jar) run via [`AuthStrategy::observe_response`].
//!
//! psflow ships four built-ins: `static_header`, `bearer`, `hmac`, `cookie_jar`.

mod apply_ctx;
mod decl;
mod error;
mod registry;
mod resolver;
mod secret;
mod state;
mod strategies;
mod strategy;

pub use apply_ctx::AuthApplyCtx;
pub use decl::AuthStrategyDecl;
pub use error::{AuthError, SecretError};
pub use registry::{AuthStrategyFactory, AuthStrategyRegistry};
pub use resolver::{NullSecretResolver, SecretRequest, SecretResolver, StaticSecretResolver};
pub use secret::SecretValue;
pub use state::{AuthState, CookieJar};
pub use strategies::{
    BearerStrategy, CookieJarStrategy, HmacStrategy, StaticHeaderStrategy, BEARER_TYPE,
    COOKIE_JAR_TYPE, HMAC_TYPE, STATIC_HEADER_TYPE,
};
pub use strategy::AuthStrategy;
