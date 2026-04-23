//! Integration tests for the WebSocket node handler.
//!
//! Spins up a lightweight `tokio-tungstenite` server for each test — small
//! enough that tests stay independent and deterministic. The debug server in
//! `src/debug_server.rs` is intentionally not reused: it is coupled to a
//! `SteppedExecutor` which would drag more surface than these tests need.

use futures::{SinkExt, StreamExt};
use psflow::auth::{AuthStrategyDecl, AuthStrategyRegistry, StaticSecretResolver};
use psflow::execute::{CancellationToken, ExecutionContext, NodeHandler, Outputs};
use psflow::graph::node::Node;
use psflow::handlers::WebSocketHandler;
use psflow::Value;
use std::net::TcpListener as StdListener;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

// -- test server --------------------------------------------------------------

/// Bind a std TCP listener to an ephemeral port, convert it into a
/// `tokio::net::TcpListener`, and return both the listener and its resolved
/// port.
///
/// Binding up front lets us hand the client a real address to connect to
/// *before* the accept loop runs — no race between the test's `connect_async`
/// and the server-side task reaching `accept()`.
fn bind_listener() -> (TcpListener, u16) {
    let std_l = StdListener::bind("127.0.0.1:0").unwrap();
    let port = std_l.local_addr().unwrap().port();
    std_l.set_nonblocking(true).unwrap();
    let tokio_l = TcpListener::from_std(std_l).unwrap();
    (tokio_l, port)
}

/// Accept a single connection on `listener` and hand it to `handler`.
async fn run_server<F, Fut>(listener: TcpListener, handler: F)
where
    F: FnOnce(Request, tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Fut
        + Send
        + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let (stream, _peer) = listener.accept().await.unwrap();
    let mut captured_req: Option<Request> = None;
    // The callback's Err arm is fixed by tungstenite's API and carries a
    // large `Response` — same reason `debug_server.rs` suppresses this lint.
    #[allow(clippy::result_large_err)]
    let ws = tokio_tungstenite::accept_hdr_async(stream, |req: &Request, resp: Response| {
        captured_req = Some(req.clone());
        Ok(resp)
    })
    .await
    .expect("handshake");
    let req = captured_req.expect("handshake callback captured request");
    handler(req, ws).await;
}

// -- output helpers -----------------------------------------------------------

fn i64_of(out: &Outputs, key: &str) -> i64 {
    match out.get(key).unwrap_or_else(|| panic!("missing key {key}")) {
        Value::I64(n) => *n,
        other => panic!("expected i64 at {key}, got {other:?}"),
    }
}

fn string_of(out: &Outputs, key: &str) -> String {
    match out.get(key).unwrap_or_else(|| panic!("missing key {key}")) {
        Value::String(s) => s.clone(),
        other => panic!("expected string at {key}, got {other:?}"),
    }
}

fn frames_of(out: &Outputs) -> Vec<Value> {
    match out.get("frames").expect("frames") {
        Value::Vec(v) => v.clone(),
        other => panic!("expected Vec frames, got {other:?}"),
    }
}

// -- tests --------------------------------------------------------------------

#[tokio::test]
async fn happy_path_collect_terminates_on_predicate() {
    let (listener, port) = bind_listener();
    let server = tokio::spawn(run_server(listener, |_req, mut ws| async move {
        for i in 0..5 {
            let body = serde_json::json!({ "seq": i, "done": i == 2 }).to_string();
            ws.send(Message::Text(body.into())).await.unwrap();
        }
        // Keep the connection open long enough for the handler to read and
        // terminate on its predicate.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = ws.close(None).await;
    }));

    let mut node = Node::new("W", "WS");
    node.config = serde_json::json!({
        "url": format!("ws://127.0.0.1:{port}/"),
        "terminate": {
            // Rhai predicate: stop when the frame's parsed JSON has done==true.
            "on_predicate": "frame.json.done == true"
        }
    });

    let out = WebSocketHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("session runs");

    server.await.ok();

    assert_eq!(string_of(&out, "terminated_by"), "predicate");
    // Counter reflects frames received up to and including the match.
    assert_eq!(i64_of(&out, "frames_received"), 3);
    assert_eq!(frames_of(&out).len(), 3);
}

#[tokio::test]
async fn max_frames_cap_terminates_session() {
    let (listener, port) = bind_listener();
    let server = tokio::spawn(run_server(listener, |_req, mut ws| async move {
        for i in 0..10 {
            ws.send(Message::Text(format!("msg-{i}").into()))
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = ws.close(None).await;
    }));

    let mut node = Node::new("W", "WS");
    node.config = serde_json::json!({
        "url": format!("ws://127.0.0.1:{port}/"),
        "terminate": { "max_frames": 4 }
    });
    let out = WebSocketHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("session");
    server.await.ok();

    assert_eq!(string_of(&out, "terminated_by"), "max_frames");
    assert_eq!(i64_of(&out, "frames_received"), 4);
}

#[tokio::test]
async fn timeout_terminates_slow_stream() {
    let (listener, port) = bind_listener();
    let server = tokio::spawn(run_server(listener, |_req, mut ws| async move {
        // Send one frame then sleep past the handler's timeout.
        ws.send(Message::Text("first".into())).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = ws.close(None).await;
    }));

    let mut node = Node::new("W", "WS");
    node.config = serde_json::json!({
        "url": format!("ws://127.0.0.1:{port}/"),
        "terminate": { "timeout_ms": 50 }
    });
    let out = WebSocketHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("session");
    server.await.ok();
    assert_eq!(string_of(&out, "terminated_by"), "timeout");
}

#[tokio::test]
async fn cancellation_terminates_session() {
    let (listener, port) = bind_listener();
    let server = tokio::spawn(run_server(listener, |_req, mut ws| async move {
        ws.send(Message::Text("keep-alive".into())).await.unwrap();
        // Do not send anything further; hold the connection open until the
        // client cancels.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = ws.close(None).await;
    }));

    let token = CancellationToken::new();
    let token_clone = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(80)).await;
        token_clone.cancel();
    });

    let mut node = Node::new("W", "WS");
    node.config = serde_json::json!({
        "url": format!("ws://127.0.0.1:{port}/"),
    });
    let out = WebSocketHandler::stateless()
        .execute(&node, Outputs::new(), token)
        .await
        .expect("session");
    server.await.ok();
    assert_eq!(string_of(&out, "terminated_by"), "cancelled");
}

#[tokio::test]
async fn init_frames_echo_round_trip() {
    let (listener, port) = bind_listener();
    let server = tokio::spawn(run_server(listener, |_req, mut ws| async move {
        // Echo-back the first N incoming frames, then close.
        let mut echoed = 0;
        while let Some(msg) = ws.next().await {
            match msg {
                Ok(Message::Text(t)) => {
                    ws.send(Message::Text(format!("echo:{t}").into()))
                        .await
                        .unwrap();
                    echoed += 1;
                    if echoed >= 2 {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        let _ = ws.close(None).await;
    }));

    let mut node = Node::new("W", "WS");
    node.config = serde_json::json!({
        "url": format!("ws://127.0.0.1:{port}/"),
        "init_frames": ["alpha", { "text": "beta" }],
        "terminate": { "max_frames": 2 }
    });
    let out = WebSocketHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("session");
    server.await.ok();

    let frames = frames_of(&out);
    assert_eq!(frames.len(), 2);
    // Extract the echoed texts.
    let mut texts = Vec::new();
    for f in frames {
        if let Value::Map(m) = f {
            if let Some(Value::String(s)) = m.get("text") {
                texts.push(s.clone());
            }
        }
    }
    assert_eq!(texts, vec!["echo:alpha", "echo:beta"]);
}

#[tokio::test]
async fn bearer_auth_header_is_applied_to_handshake() {
    let (listener, port) = bind_listener();
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let server = tokio::spawn(run_server(listener, move |req, mut ws| async move {
        let auth = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let _ = tx.send(auth);
        ws.send(Message::Text("ok".into())).await.unwrap();
        let _ = ws.close(None).await;
    }));

    // Graph-level auth: bearer strategy with secret resolution.
    let resolver = Arc::new(StaticSecretResolver::new());
    resolver.insert_flat("my_token", "sekret");
    let mut ctx = ExecutionContext::new();
    ctx.set_secret_resolver(resolver);
    let ctx = Arc::new(ctx);

    let mut registry = AuthStrategyRegistry::with_builtins();
    let mut decls = std::collections::BTreeMap::new();
    decls.insert(
        "api".into(),
        AuthStrategyDecl::new("bearer").with_secret("token", "my_token"),
    );
    registry.build_from_decls(&decls).unwrap();
    ctx.install_auth_registry(registry);

    let mut node = Node::new("W", "WS");
    node.config = serde_json::json!({
        "url": format!("ws://127.0.0.1:{port}/"),
        "auth": "api",
        "terminate": { "max_frames": 1 }
    });
    let out = WebSocketHandler::new(ctx)
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("session");
    let header = rx.await.unwrap();
    server.await.ok();
    assert_eq!(header, "Bearer sekret");
    assert_eq!(i64_of(&out, "frames_received"), 1);
}

#[tokio::test]
async fn validation_fail_mode_fails_node_on_invalid_frame() {
    let (listener, port) = bind_listener();
    let server = tokio::spawn(run_server(listener, |_req, mut ws| async move {
        // The schema requires `id`; this frame omits it.
        ws.send(Message::Text("{\"no_id\": true}".into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = ws.close(None).await;
    }));

    let mut node = Node::new("W", "WS");
    node.config = serde_json::json!({
        "url": format!("ws://127.0.0.1:{port}/"),
        "validation": {
            "inline": { "type": "object", "required": ["id"] },
            "on_failure": "fail"
        },
        "terminate": { "max_frames": 5 }
    });
    let result = WebSocketHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await;
    server.await.ok();
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("schema validation failed"),
        "expected schema-failure message, got: {err}"
    );
}

#[tokio::test]
async fn validation_passthrough_mode_attaches_error_to_frame() {
    let (listener, port) = bind_listener();
    let server = tokio::spawn(run_server(listener, |_req, mut ws| async move {
        ws.send(Message::Text("{\"no_id\": true}".into()))
            .await
            .unwrap();
        ws.send(Message::Text("{\"id\": 1, \"done\": true}".into()))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = ws.close(None).await;
    }));

    let mut node = Node::new("W", "WS");
    node.config = serde_json::json!({
        "url": format!("ws://127.0.0.1:{port}/"),
        "validation": {
            "inline": { "type": "object", "required": ["id"] },
            "on_failure": "passthrough"
        },
        "terminate": { "on_predicate": "frame.json.done == true" }
    });
    let out = WebSocketHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("session");
    server.await.ok();

    assert_eq!(string_of(&out, "terminated_by"), "predicate");
    let frames = frames_of(&out);
    assert_eq!(frames.len(), 2);
    // First frame carries a validation_error; second does not.
    match &frames[0] {
        Value::Map(m) => assert!(m.contains_key("validation_error")),
        other => panic!("expected Map, got {other:?}"),
    }
    match &frames[1] {
        Value::Map(m) => assert!(!m.contains_key("validation_error")),
        other => panic!("expected Map, got {other:?}"),
    }
}

#[tokio::test]
async fn emit_sink_file_writes_frames_one_per_line() {
    let (listener, port) = bind_listener();
    let server = tokio::spawn(run_server(listener, |_req, mut ws| async move {
        for i in 0..3 {
            ws.send(Message::Text(format!("{{\"seq\":{i}}}").into()))
                .await
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = ws.close(None).await;
    }));

    let tmp = tempfile::tempdir().unwrap();
    let out_path = tmp.path().join("frames.ndjson");

    let mut node = Node::new("W", "WS");
    node.config = serde_json::json!({
        "url": format!("ws://127.0.0.1:{port}/"),
        "terminate": { "max_frames": 3 },
        "emit": {
            "sink_file": { "path": out_path.to_str().unwrap() }
        }
    });
    let out = WebSocketHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("session");
    server.await.ok();

    assert_eq!(i64_of(&out, "frames_received"), 3);
    assert_eq!(string_of(&out, "terminated_by"), "max_frames");
    assert_eq!(string_of(&out, "path"), out_path.to_string_lossy());
    // Output has no frames field — we streamed.
    assert!(!out.contains_key("frames"));

    let on_disk = std::fs::read_to_string(&out_path).unwrap();
    let lines: Vec<&str> = on_disk.lines().collect();
    assert_eq!(lines.len(), 3);
    for (i, line) in lines.iter().enumerate() {
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(parsed["kind"], "text");
        assert_eq!(parsed["json"]["seq"], i);
    }
}

#[tokio::test]
async fn hmac_strategy_is_rejected_for_ws_at_graph_load() {
    // Register an HMAC strategy, declare it in the graph, and attach it to a
    // WS node. `validate_graph` must refuse.
    use psflow::graph::Graph;
    let mut graph = Graph::new();
    graph.metadata_mut().auth.insert(
        "sig".into(),
        AuthStrategyDecl::new("hmac")
            .with_secret("key_id", "k")
            .with_secret("secret", "s"),
    );
    let mut node = Node::new("W", "WS");
    node.handler = Some("ws".into());
    node.config = serde_json::json!({
        "url": "ws://example.com/",
        "auth": "sig",
    });
    graph.add_node(node).unwrap();

    let reg = AuthStrategyRegistry::with_builtins();
    let err = reg.validate_graph(&graph).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("does not support") && msg.contains("WebSocket"),
        "expected WS-compat error, got: {msg}"
    );
}
