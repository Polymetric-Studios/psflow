//! Integration tests for the graph-scoped auth strategy layer.
//!
//! These run an actual HTTP handler against a `wiremock` stub server to
//! verify that:
//! - `config.auth` references a declared strategy
//! - the strategy's `apply` hook injects the right headers
//! - missing/undeclared strategies surface as validation errors

use psflow::auth::{
    AuthApplyCtx, AuthError, AuthStrategy, AuthStrategyDecl, AuthStrategyRegistry, SecretResolver,
    StaticSecretResolver,
};
use psflow::execute::{CancellationToken, ExecutionContext, NodeHandler, Outputs};
use psflow::graph::node::Node;
use psflow::graph::Graph;
use psflow::handlers::HttpHandler;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn make_ctx_with_resolver(resolver: Arc<dyn SecretResolver>) -> Arc<ExecutionContext> {
    let mut ctx = ExecutionContext::new();
    ctx.set_secret_resolver(resolver);
    Arc::new(ctx)
}

#[tokio::test]
async fn bearer_auth_injects_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/widgets"))
        .and(header("authorization", "Bearer super-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_string("OK"))
        .mount(&server)
        .await;

    // Host-side: register secret `api_key = super-secret`.
    let resolver = Arc::new(StaticSecretResolver::new());
    resolver.insert_flat("api_key", "super-secret");
    let ctx = make_ctx_with_resolver(resolver);

    // Graph-level declaration of a bearer strategy named `api_auth`.
    let mut registry = AuthStrategyRegistry::with_builtins();
    let mut decls = std::collections::BTreeMap::new();
    decls.insert(
        "api_auth".into(),
        AuthStrategyDecl::new("bearer").with_secret("token", "api_key"),
    );
    registry.build_from_decls(&decls).unwrap();
    ctx.install_auth_registry(registry);

    // Node references the strategy by name. `allow_private` lets us target localhost.
    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": format!("{}/v1/widgets", server.uri()),
        "method": "GET",
        "allow_private": true,
        "auth": "api_auth",
    });

    let handler = HttpHandler::new(ctx);
    let out = handler
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .unwrap();
    let status = out.get("status").unwrap();
    match status {
        psflow::Value::I64(n) => assert_eq!(*n, 200),
        other => panic!("expected i64 status, got {other:?}"),
    }
}

#[tokio::test]
async fn static_header_auth_injects_custom_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ping"))
        .and(header("x-api-key", "abc123"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let resolver = Arc::new(StaticSecretResolver::new());
    let ctx = make_ctx_with_resolver(resolver);
    let mut registry = AuthStrategyRegistry::with_builtins();
    let mut decls = std::collections::BTreeMap::new();
    decls.insert(
        "apikey".into(),
        AuthStrategyDecl::new("static_header").with_params(serde_json::json!({
            "name": "X-Api-Key",
            "value": "abc123",
        })),
    );
    registry.build_from_decls(&decls).unwrap();
    ctx.install_auth_registry(registry);

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": format!("{}/ping", server.uri()),
        "allow_private": true,
        "auth": "apikey",
    });

    let handler = HttpHandler::new(ctx);
    let out = handler
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .unwrap();
    match out.get("status").unwrap() {
        psflow::Value::I64(n) => assert_eq!(*n, 204),
        other => panic!("expected i64, got {other:?}"),
    }
}

#[tokio::test]
async fn undeclared_strategy_fails_at_validation() {
    // Graph references a strategy name that was never declared.
    let mut graph = Graph::new();
    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": "http://example.com",
        "auth": "ghost",
    });
    graph.add_node(node).unwrap();

    let registry = AuthStrategyRegistry::with_builtins();
    let err = registry.validate_graph(&graph).unwrap_err();
    assert!(matches!(
        err,
        psflow::auth::AuthError::UndeclaredStrategy { .. }
    ));
}

#[tokio::test]
async fn missing_role_fails_at_validation() {
    // Bearer requires role `token`; decl omits it.
    let mut graph = Graph::new();
    graph
        .metadata_mut()
        .auth
        .insert("bad".into(), AuthStrategyDecl::new("bearer"));

    let registry = AuthStrategyRegistry::with_builtins();
    let err = registry.validate_graph(&graph).unwrap_err();
    assert!(matches!(err, psflow::auth::AuthError::MissingRole { .. }));
}

#[tokio::test]
async fn unknown_strategy_type_fails_at_validation() {
    let mut graph = Graph::new();
    graph
        .metadata_mut()
        .auth
        .insert("oddball".into(), AuthStrategyDecl::new("no_such_type"));

    let registry = AuthStrategyRegistry::with_builtins();
    let err = registry.validate_graph(&graph).unwrap_err();
    assert!(matches!(
        err,
        psflow::auth::AuthError::UnknownStrategyType { .. }
    ));
}

#[tokio::test]
async fn cookie_jar_absorbs_and_resends() {
    let server = MockServer::start().await;
    // First request: server issues a cookie.
    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(
            ResponseTemplate::new(200).append_header("set-cookie", "session=abc123; Path=/"),
        )
        .mount(&server)
        .await;
    // Second request: expect the session cookie.
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("cookie", "session=abc123"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let resolver = Arc::new(StaticSecretResolver::new());
    let ctx = make_ctx_with_resolver(resolver);
    let mut registry = AuthStrategyRegistry::with_builtins();
    let mut decls = std::collections::BTreeMap::new();
    decls.insert("jar".into(), AuthStrategyDecl::new("cookie_jar"));
    registry.build_from_decls(&decls).unwrap();
    ctx.install_auth_registry(registry);

    let handler = HttpHandler::new(ctx.clone());

    // Hit /login first.
    let mut login = Node::new("L", "Login");
    login.config = serde_json::json!({
        "url": format!("{}/login", server.uri()),
        "allow_private": true,
        "auth": "jar",
    });
    let _ = handler
        .execute(&login, Outputs::new(), CancellationToken::new())
        .await
        .unwrap();

    // Then /me — cookie must be attached.
    let mut me = Node::new("M", "Me");
    me.config = serde_json::json!({
        "url": format!("{}/me", server.uri()),
        "allow_private": true,
        "auth": "jar",
    });
    let out = handler
        .execute(&me, Outputs::new(), CancellationToken::new())
        .await
        .unwrap();
    match out.get("status").unwrap() {
        psflow::Value::I64(n) => assert_eq!(*n, 200),
        other => panic!("expected i64, got {other:?}"),
    }
}

/// A test-only strategy that counts apply / observe_response invocations.
/// Also injects a rotating bearer token so we can assert rotation across
/// retry attempts.
struct CountingStrategy {
    apply_count: Arc<AtomicU32>,
    observe_count: Arc<AtomicU32>,
}

#[async_trait::async_trait]
impl AuthStrategy for CountingStrategy {
    fn type_name(&self) -> &'static str {
        "counting"
    }

    async fn apply(
        &self,
        _ctx: &AuthApplyCtx<'_>,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, AuthError> {
        let n = self.apply_count.fetch_add(1, Ordering::SeqCst) + 1;
        // Per-attempt rotating header — proves apply() is re-invoked each try.
        Ok(builder.header("x-attempt", n.to_string()))
    }

    async fn observe_response(
        &self,
        _ctx: &AuthApplyCtx<'_>,
        _headers: &reqwest::header::HeaderMap,
    ) -> Result<(), AuthError> {
        self.observe_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn retry_reinvokes_auth_apply_and_observe_each_attempt() {
    let server = MockServer::start().await;
    // First attempt sees x-attempt: 1 and gets a 503.
    Mock::given(method("GET"))
        .and(path("/rotate"))
        .and(header("x-attempt", "1"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Second attempt sees x-attempt: 2 and gets a 200.
    Mock::given(method("GET"))
        .and(path("/rotate"))
        .and(header("x-attempt", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let apply_count = Arc::new(AtomicU32::new(0));
    let observe_count = Arc::new(AtomicU32::new(0));

    // Register the counting strategy under its own type.
    let resolver = Arc::new(StaticSecretResolver::new());
    let ctx = make_ctx_with_resolver(resolver);

    let mut registry = AuthStrategyRegistry::with_builtins();
    let apply_c = apply_count.clone();
    let observe_c = observe_count.clone();
    registry.register_factory(
        "counting",
        Arc::new(move |_decl| {
            let s: Arc<dyn AuthStrategy> = Arc::new(CountingStrategy {
                apply_count: apply_c.clone(),
                observe_count: observe_c.clone(),
            });
            Ok(s)
        }),
    );
    let mut decls = std::collections::BTreeMap::new();
    decls.insert("tally".into(), AuthStrategyDecl::new("counting"));
    registry.build_from_decls(&decls).unwrap();
    ctx.install_auth_registry(registry);

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": format!("{}/rotate", server.uri()),
        "allow_private": true,
        "auth": "tally",
        "retry": {
            "max_attempts": 3,
            "backoff": "fixed",
            "delay_ms": 1,
            "retry_on": ["5xx"]
        }
    });

    let handler = HttpHandler::new(ctx);
    let out = handler
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .unwrap();

    match out.get("status").unwrap() {
        psflow::Value::I64(n) => assert_eq!(*n, 200),
        other => panic!("expected i64, got {other:?}"),
    }

    // apply() must be called once per attempt (both the 503 and the 200).
    assert_eq!(apply_count.load(Ordering::SeqCst), 2);
    // observe_response() must also run on the failed 503, not only the final 200.
    assert_eq!(observe_count.load(Ordering::SeqCst), 2);
}
