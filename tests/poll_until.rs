//! Integration tests for the `poll_until` node handler.
//!
//! Covers:
//! - Predicate satisfied on first attempt (no delay fires).
//! - Predicate satisfied on third attempt (elapsed >= 2 * delay_ms,
//!   attempts_used == 3).
//! - Max attempts hit without predicate match — `timed_out=true`, output is
//!   the last subgraph output.
//! - Cancellation mid-poll (sent during a sleep) exits cleanly.
//! - Subgraph failure propagates as a node failure.

use psflow::execute::{sync_handler, CancellationToken, HandlerRegistry, NodeHandler, Outputs};
use psflow::graph::node::Node;
use psflow::graph::Graph;
use psflow::handlers::poll_until::PollUntilHandler;
use psflow::handlers::subgraph_invoke::GraphLibrary;
use psflow::scripting::engine::default_script_engine;
use psflow::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// -- test graph builders ------------------------------------------------------

/// Build a two-node graph: INPUT (source, replaced by input injection) →
/// WORKER (sink). The WORKER handler is whatever is registered under
/// `worker_handler_name`.
fn worker_graph(worker_handler_name: &str) -> Graph {
    let mut g = Graph::new();
    g.add_node(Node::new("INPUT", "Input").with_handler("pass"))
        .unwrap();
    g.add_node(Node::new("WORKER", "Worker").with_handler(worker_handler_name))
        .unwrap();
    g.add_edge(&"INPUT".into(), "", &"WORKER".into(), "", None)
        .unwrap();
    g
}

fn make_pass_handlers() -> HandlerRegistry {
    let mut h = HandlerRegistry::new();
    h.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
    h
}

// -- output helpers ----------------------------------------------------------

fn i64_of(out: &Outputs, key: &str) -> i64 {
    match out.get(key).unwrap_or_else(|| panic!("missing key {key}")) {
        Value::I64(n) => *n,
        other => panic!("expected i64 at {key}, got {other:?}"),
    }
}

fn bool_of(out: &Outputs, key: &str) -> bool {
    match out.get(key).unwrap_or_else(|| panic!("missing key {key}")) {
        Value::Bool(b) => *b,
        other => panic!("expected bool at {key}, got {other:?}"),
    }
}

fn output_map(out: &Outputs) -> BTreeMap<String, Value> {
    match out.get("output").expect("missing `output`") {
        Value::Map(m) => m.clone(),
        other => panic!("expected Map at `output`, got {other:?}"),
    }
}

// -- tests -------------------------------------------------------------------

#[tokio::test]
async fn predicate_satisfied_first_attempt_no_delay() {
    let mut lib = GraphLibrary::new();
    lib.register("g", worker_graph("worker"));

    let mut handlers = make_pass_handlers();
    handlers.insert(
        "worker".into(),
        sync_handler(|_, _| {
            let mut out = Outputs::new();
            out.insert("ready".into(), Value::Bool(true));
            Ok(out)
        }),
    );

    let engine = default_script_engine();
    let h = PollUntilHandler::with_handlers(Arc::new(lib), handlers, engine);

    let mut node = Node::new("P", "Poll");
    node.config = serde_json::json!({
        "graph": "g",
        "predicate": "output.ready == true",
        "max_attempts": 5,
        "delay_ms": 500,
    });

    let start = Instant::now();
    let outputs = h
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("poll_until should succeed");
    let elapsed = start.elapsed();

    assert_eq!(i64_of(&outputs, "attempts_used"), 1);
    assert!(!bool_of(&outputs, "timed_out"));
    let out_map = output_map(&outputs);
    assert_eq!(out_map.get("ready"), Some(&Value::Bool(true)));

    // No leading delay fires before the first attempt — well below delay_ms.
    assert!(
        elapsed < Duration::from_millis(300),
        "first-attempt match should not sleep; elapsed={elapsed:?}"
    );
}

#[tokio::test]
async fn predicate_satisfied_on_third_attempt() {
    let counter = Arc::new(AtomicI64::new(0));
    let counter_for_handler = counter.clone();

    let mut lib = GraphLibrary::new();
    lib.register("g", worker_graph("worker"));

    let mut handlers = make_pass_handlers();
    handlers.insert(
        "worker".into(),
        sync_handler(move |_, _| {
            let n = counter_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
            let mut out = Outputs::new();
            out.insert("n".into(), Value::I64(n));
            Ok(out)
        }),
    );

    let engine = default_script_engine();
    let h = PollUntilHandler::with_handlers(Arc::new(lib), handlers, engine);

    let mut node = Node::new("P", "Poll");
    // Fires true when worker has returned three times (counter starts at 1).
    node.config = serde_json::json!({
        "graph": "g",
        "predicate": "output.n >= 3",
        "max_attempts": 10,
        "delay_ms": 50,
    });

    let start = Instant::now();
    let outputs = h
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("poll_until should succeed");
    let elapsed = start.elapsed();

    assert_eq!(i64_of(&outputs, "attempts_used"), 3);
    assert!(!bool_of(&outputs, "timed_out"));
    let out_map = output_map(&outputs);
    assert_eq!(out_map.get("n"), Some(&Value::I64(3)));

    // Two delays between three attempts => at least 2 * 50 = 100 ms. Allow
    // small scheduling slack on the low side.
    assert!(
        elapsed >= Duration::from_millis(90),
        "expected >= 2 * delay_ms elapsed; got {elapsed:?}"
    );
}

#[tokio::test]
async fn max_attempts_reached_times_out() {
    let counter = Arc::new(AtomicI64::new(0));
    let counter_for_handler = counter.clone();

    let mut lib = GraphLibrary::new();
    lib.register("g", worker_graph("worker"));

    let mut handlers = make_pass_handlers();
    handlers.insert(
        "worker".into(),
        sync_handler(move |_, _| {
            let n = counter_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
            let mut out = Outputs::new();
            out.insert("n".into(), Value::I64(n));
            Ok(out)
        }),
    );

    let engine = default_script_engine();
    let h = PollUntilHandler::with_handlers(Arc::new(lib), handlers, engine);

    let mut node = Node::new("P", "Poll");
    node.config = serde_json::json!({
        "graph": "g",
        "predicate": "output.n >= 100",
        "max_attempts": 3,
        "delay_ms": 10,
    });

    let outputs = h
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("poll_until should succeed-with-timeout, not fail");

    assert_eq!(i64_of(&outputs, "attempts_used"), 3);
    assert!(bool_of(&outputs, "timed_out"));
    let out_map = output_map(&outputs);
    // The last subgraph output — the third invocation returned n=3.
    assert_eq!(out_map.get("n"), Some(&Value::I64(3)));
}

#[tokio::test]
async fn cancellation_mid_poll_exits_cleanly() {
    let mut lib = GraphLibrary::new();
    lib.register("g", worker_graph("worker"));

    let mut handlers = make_pass_handlers();
    handlers.insert(
        "worker".into(),
        sync_handler(|_, _| {
            let mut out = Outputs::new();
            out.insert("ready".into(), Value::Bool(false));
            Ok(out)
        }),
    );

    let engine = default_script_engine();
    let h = PollUntilHandler::with_handlers(Arc::new(lib), handlers, engine);

    let mut node = Node::new("P", "Poll");
    // A predicate that never fires, with a delay long enough that the cancel
    // arrives during the inter-attempt sleep.
    node.config = serde_json::json!({
        "graph": "g",
        "predicate": "output.ready == true",
        "max_attempts": 100,
        "delay_ms": 200,
    });

    let token = CancellationToken::new();
    let token_clone = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        token_clone.cancel();
    });

    let start = Instant::now();
    let err = h
        .execute(&node, Outputs::new(), token)
        .await
        .expect_err("cancellation should propagate as an error");
    let elapsed = start.elapsed();

    let msg = err.to_string();
    assert!(
        msg.contains("cancel"),
        "expected cancellation error, got: {msg}"
    );
    // Must return before the full 200ms delay (let alone any further attempts).
    assert!(
        elapsed < Duration::from_millis(400),
        "cancellation should fire promptly; elapsed={elapsed:?}"
    );
}

#[tokio::test]
async fn subgraph_invocation_error_propagates() {
    // A subgraph_invoke-level failure surfaces as a NodeError out of the
    // inner handler. We trigger one by registering a subgraph that the
    // poll_until handler's inner `SubgraphInvocationHandler` cannot resolve
    // — `config.graph` is enforced at load time, but an uninitialised
    // downstream handler registry (via the deferred-slot constructor) gives
    // us a clean, deterministic invoke-level error after the subgraph
    // *does* exist.
    let mut lib = GraphLibrary::new();
    lib.register("g", worker_graph("worker"));

    let engine = default_script_engine();
    let (h, _slot) = PollUntilHandler::new(Arc::new(lib), engine);
    // Intentionally do NOT call slot.set — the handler registry the inner
    // SubgraphInvocationHandler relies on is never initialised, so any
    // attempt to invoke the subgraph errors.

    let mut node = Node::new("P", "Poll");
    node.config = serde_json::json!({
        "graph": "g",
        "predicate": "output.ready == true",
        "max_attempts": 5,
        "delay_ms": 0,
    });

    let err = h
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect_err("uninitialized registry should surface as an error");
    assert!(err.to_string().contains("not initialized"));
}

#[tokio::test]
async fn predicate_runtime_error_propagates() {
    // A runtime error inside the predicate (here: indexing a missing map
    // field with strict-access semantics via `type_of`) surfaces as a node
    // failure referencing config.predicate.
    let mut lib = GraphLibrary::new();
    lib.register("g", worker_graph("worker"));

    let mut handlers = make_pass_handlers();
    handlers.insert(
        "worker".into(),
        sync_handler(|_, _| {
            let mut out = Outputs::new();
            out.insert("n".into(), Value::I64(1));
            Ok(out)
        }),
    );

    let engine = default_script_engine();
    let h = PollUntilHandler::with_handlers(Arc::new(lib), handlers, engine);

    let mut node = Node::new("P", "Poll");
    // `1 / 0` is a Rhai runtime error; evaluating it fails mid-loop.
    node.config = serde_json::json!({
        "graph": "g",
        "predicate": "(1 / 0) == 0",
        "max_attempts": 5,
        "delay_ms": 0,
    });

    let err = h
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect_err("predicate runtime error should propagate");
    assert!(
        err.to_string().contains("predicate"),
        "expected predicate error, got: {err}"
    );
}
