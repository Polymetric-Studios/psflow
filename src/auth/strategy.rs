use super::apply_ctx::AuthApplyCtx;
use super::error::AuthError;
use async_trait::async_trait;
use reqwest::RequestBuilder;

/// The auth strategy contract.
///
/// One concrete implementation per authentication scheme. The same type
/// can be instantiated multiple times under different graph-local names
/// (e.g. two bearer strategies pointing at different host secrets).
///
/// ## Transport surfaces
///
/// Strategies ship with two parallel seams:
///
/// - [`AuthStrategy::apply`] for the HTTP handler, operating on a
///   `reqwest::RequestBuilder`.
/// - [`AuthStrategy::apply_ws_request`] for the WebSocket handler, operating
///   on the `http::Request<()>` used by the `tokio-tungstenite` handshake.
///   Default impl returns [`AuthError::Apply`] — strategies that meaningfully
///   translate to a WS upgrade request override. Callers should gate on
///   [`AuthStrategy::supports_ws`] at graph-load time rather than hitting the
///   runtime error.
///
/// ## Built-in WS support matrix
///
/// | Strategy       | HTTP | WS  |
/// |----------------|:----:|:---:|
/// | static_header  |  ✓   |  ✓  |
/// | bearer         |  ✓   |  ✓  |
/// | cookie_jar     |  ✓   |  ✓  |
/// | hmac           |  ✓   |  —  |
///
/// HMAC is intentionally unsupported on WS: the canonicalisation signs a
/// `METHOD\nPATH\nHEADERS\nBODYHASH` tuple that has no analogue on a
/// handshake-only GET with no body.
#[async_trait]
pub trait AuthStrategy: Send + Sync {
    /// The discriminator this strategy type registers under.
    ///
    /// Used by the registry to route declarations to the right factory.
    /// Instances produced by factories do not need to match this on a per-
    /// instance basis — it is informational.
    fn type_name(&self) -> &'static str;

    /// Role names the strategy requires in the declaration's `secrets` map.
    ///
    /// Checked at graph load time by
    /// [`super::AuthStrategyRegistry::validate_decl`]. Strategies that need
    /// no secrets return an empty slice.
    fn required_roles(&self) -> &'static [&'static str] {
        &[]
    }

    /// True if this strategy can inject auth into a WebSocket handshake
    /// (i.e. [`AuthStrategy::apply_ws_request`] is a real implementation).
    ///
    /// The default is `false` so strategies that are HTTP-only opt in
    /// explicitly when they grow WS support.
    fn supports_ws(&self) -> bool {
        false
    }

    /// Mutate the outgoing request. Called after the handler has set the
    /// method, URL, headers, and body.
    async fn apply(
        &self,
        ctx: &AuthApplyCtx<'_>,
        builder: RequestBuilder,
    ) -> Result<RequestBuilder, AuthError>;

    /// WebSocket-handshake variant of [`apply`].
    ///
    /// The WS handler uses `tokio-tungstenite`, which performs the handshake
    /// on an `http::Request<()>` builder rather than a `reqwest::RequestBuilder`.
    /// Strategies that only decorate headers (static_header, bearer, cookie_jar)
    /// override this; signing-style strategies (hmac) leave the default.
    ///
    /// The default impl returns an [`AuthError::Apply`] with a clear message
    /// so a misconfiguration that slips past load-time validation still fails
    /// cleanly at handshake time.
    async fn apply_ws_request(
        &self,
        ctx: &AuthApplyCtx<'_>,
        _request: http::Request<()>,
    ) -> Result<http::Request<()>, AuthError> {
        Err(AuthError::Apply {
            name: ctx.strategy_name.to_string(),
            message: format!(
                "auth strategy type '{}' does not support WebSocket handshakes",
                self.type_name()
            ),
        })
    }

    /// Post-response hook — runs after the handler receives the response
    /// and before body extraction. The default is a no-op; cookie-jar-
    /// style strategies override to absorb `Set-Cookie`.
    ///
    /// Receives a reference to the response headers plus the apply-ctx so
    /// the strategy can update per-run state.
    async fn observe_response(
        &self,
        _ctx: &AuthApplyCtx<'_>,
        _headers: &reqwest::header::HeaderMap,
    ) -> Result<(), AuthError> {
        Ok(())
    }
}
