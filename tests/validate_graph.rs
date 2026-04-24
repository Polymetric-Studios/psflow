//! Integration tests for the graph-load validation pass.
//!
//! Verifies that a graph with multiple misconfigured nodes surfaces every
//! issue at once on executor start-up, before any node runs.

use std::sync::Arc;

use psflow::execute::{
    validation::validate_graph, CancellationToken, ExecutionContext, Executor, HandlerRegistry,
    TopologicalExecutor,
};
use psflow::handlers::{GraphLibrary, HttpHandler, PollUntilHandler};
use psflow::{ExecutionError, Graph, Node, NodeRegistry};

fn script_engine() -> Arc<psflow::scripting::engine::ScriptEngine> {
    psflow::scripting::engine::default_script_engine()
}

/// A graph with three separately-bad nodes surfaces all three issues at
/// once when the executor runs it.
#[tokio::test]
async fn multi_issue_graph_fails_on_executor_startup() {
    let mut graph = Graph::new();

    // Bad node 1: http with body_sink + validation incompat.
    let mut http_node = Node::new("H", "Http").with_handler("http");
    http_node.config = serde_json::json!({
        "url": "http://example.com",
        "body_sink": { "file": { "path": "/tmp/out.bin" } },
        "validation": { "inline": { "type": "object" } }
    });
    graph.add_node(http_node).unwrap();

    // Bad node 2: poll_until with missing subgraph.
    let mut poll_node = Node::new("P", "Poll").with_handler("poll_until");
    poll_node.config = serde_json::json!({
        "graph": "no_such_subgraph",
        "predicate": "true",
        "max_attempts": 1,
        "delay_ms": 0,
    });
    graph.add_node(poll_node).unwrap();

    // Bad node 3: ws with a bad predicate (script compile failure).
    let mut ws_node = Node::new("W", "Ws").with_handler("ws");
    ws_node.config = serde_json::json!({
        "url": "wss://example.com",
        "terminate": { "on_predicate": "let x = ;; garbage" }
    });
    graph.add_node(ws_node).unwrap();

    // Handler registry with the three handlers we care about plus poll_until.
    let engine = script_engine();
    let mut reg = NodeRegistry::with_defaults(engine.clone());
    let lib = Arc::new(GraphLibrary::new());
    let poll = PollUntilHandler::with_handlers(lib, HandlerRegistry::new(), engine);
    reg.register(
        "poll_until",
        Arc::new(poll) as Arc<dyn psflow::execute::NodeHandler>,
    );

    let handlers = reg.into_handler_registry();

    let exec = TopologicalExecutor::new();
    let err = exec
        .execute(&graph, &handlers)
        .await
        .expect_err("executor must refuse to run a graph with multiple misconfigured nodes");

    let ExecutionError::ValidationFailed(msg) = err else {
        panic!("expected ValidationFailed, got {err:?}");
    };

    // Every issue is surfaced in the same error.
    assert!(msg.contains("mutually exclusive"), "http issue: {msg}");
    assert!(msg.contains("no_such_subgraph"), "poll_until issue: {msg}");
    assert!(msg.contains("on_predicate"), "ws issue: {msg}");
    // Aggregator reports the total count up front.
    assert!(msg.contains("3 graph validation"), "count in header: {msg}");
}

/// Calling `validate_graph` on a clean graph returns Ok even when the
/// graph uses handlers that implement the hook.
#[tokio::test]
async fn clean_graph_passes_validation() {
    let mut graph = Graph::new();

    let mut node = Node::new("H", "Http").with_handler("http");
    node.config = serde_json::json!({ "url": "http://example.com" });
    graph.add_node(node).unwrap();

    let handlers: HandlerRegistry = {
        let mut m = std::collections::HashMap::new();
        m.insert(
            "http".into(),
            Arc::new(HttpHandler::stateless()) as Arc<dyn psflow::execute::NodeHandler>,
        );
        m
    };
    let ctx = ExecutionContext::new();

    validate_graph(&graph, &handlers, &ctx).expect("clean graph must validate");
}

/// Direct-path sanity: handlers with no validate_node implementation are
/// treated as clean by the pass.
#[tokio::test]
async fn handler_without_override_is_clean() {
    let mut graph = Graph::new();
    graph
        .add_node(Node::new("A", "A").with_handler("passthrough"))
        .unwrap();

    let engine = script_engine();
    let reg = NodeRegistry::with_defaults(engine);
    let handlers = reg.into_handler_registry();
    let ctx = ExecutionContext::new();

    validate_graph(&graph, &handlers, &ctx).expect("default impl is Ok");

    // Cancellation-token param is not needed here, kept to ensure the
    // CancellationToken is re-exported in the public surface used by
    // embedders mirroring this test.
    let _tok = CancellationToken::new();
}
