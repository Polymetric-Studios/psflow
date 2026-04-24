//! `poll_until` node handler.
//!
//! Invokes a named subgraph on a fixed-delay loop until a Rhai predicate over
//! the subgraph's output map returns true, or a `max_attempts` cap is hit.
//!
//! Scope is deliberately minimal (see
//! `ergon/active-documents/20260423-165256-polling-compose-first-findings.md`).
//! Exactly four config keys:
//!
//! - `graph` (string, required) — subgraph name to invoke per attempt.
//! - `predicate` (string, required) — Rhai expression evaluated over
//!   `output` (the subgraph's output map) and `attempt` (1-indexed u32).
//!   Truthy → the poll terminates successfully.
//! - `max_attempts` (u32, required) — hard cap. When hit without the predicate
//!   matching, terminate with `timed_out: true` (NOT a node failure).
//! - `delay_ms` (u64, required) — fixed delay between attempts. First attempt
//!   fires immediately (no leading delay).
//!
//! Outputs (exactly three keys):
//!
//! - `attempts_used` (i64) — number of attempts performed.
//! - `timed_out` (bool) — true iff `max_attempts` was reached without the
//!   predicate matching.
//! - `output` (map) — the final subgraph output (predicate match or last
//!   attempt before cap).
//!
//! Not in scope (intentional deferrals): exponential or custom backoff,
//! jitter, per-attempt timeout, retry-on-error, fail-on-cap toggle, initial
//! delay, custom cancellation policy. The subgraph itself owns per-attempt
//! timeout; a node-level retry wrapper owns retry-on-error. Callers branch
//! on `timed_out`.

use crate::error::NodeError;
use crate::execute::validation::{ValidationIssue, ValidationIssueKind};
use crate::execute::{
    CancellationToken, ExecutionContext, HandlerRegistry, HandlerSchema, NodeHandler, Outputs,
    SchemaField,
};
use crate::graph::node::{Node, NodeId};
use crate::graph::types::Value;
use crate::graph::Graph;
use crate::handlers::subgraph_invoke::{GraphLibrary, SubgraphInvocationHandler};
use crate::scripting::bridge::value_to_dynamic;
use crate::scripting::engine::ScriptEngine;
use rhai::{Dynamic, Scope, AST};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The registry name for this handler.
pub const POLL_UNTIL_HANDLER_NAME: &str = "poll_until";

/// Parsed `poll_until` config — exactly the four keys the findings doc permits.
#[derive(Debug)]
struct PollUntilConfig {
    graph_name: String,
    predicate_src: String,
    max_attempts: u32,
    delay_ms: u64,
}

impl PollUntilConfig {
    fn from_json(config: &serde_json::Value) -> Result<Self, String> {
        let graph_name = config
            .get("graph")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing config.graph (string)".to_string())?
            .to_string();
        let predicate_src = config
            .get("predicate")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing config.predicate (string)".to_string())?
            .to_string();
        let max_attempts_raw = config
            .get("max_attempts")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing config.max_attempts (positive integer)".to_string())?;
        if max_attempts_raw == 0 {
            return Err("config.max_attempts must be >= 1".to_string());
        }
        if max_attempts_raw > u32::MAX as u64 {
            return Err(format!(
                "config.max_attempts {max_attempts_raw} exceeds u32::MAX"
            ));
        }
        let max_attempts = max_attempts_raw as u32;
        let delay_ms = config
            .get("delay_ms")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing config.delay_ms (non-negative integer)".to_string())?;
        Ok(Self {
            graph_name,
            predicate_src,
            max_attempts,
            delay_ms,
        })
    }
}

/// Handler for `poll_until` — attempt-loop wrapper over a subgraph invocation.
pub struct PollUntilHandler {
    library: Arc<GraphLibrary>,
    handlers: Arc<std::sync::OnceLock<HandlerRegistry>>,
    exec_ctx: Option<Arc<ExecutionContext>>,
    script_engine: Arc<ScriptEngine>,
    /// Compiled predicate ASTs, keyed by source string. Shared across
    /// invocations so a repeatedly-executed node compiles its predicate once.
    predicate_asts: Arc<Mutex<HashMap<String, Arc<AST>>>>,
}

impl PollUntilHandler {
    /// Build a handler with an already-initialised handler registry (the
    /// registry the inner subgraph invocation will run child graphs under).
    pub fn with_handlers(
        library: Arc<GraphLibrary>,
        handlers: HandlerRegistry,
        script_engine: Arc<ScriptEngine>,
    ) -> Self {
        let slot = Arc::new(std::sync::OnceLock::new());
        slot.set(handlers).ok();
        Self {
            library,
            handlers: slot,
            exec_ctx: None,
            script_engine,
            predicate_asts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Build a handler with deferred handler-registry initialisation. Use
    /// this when the registry must contain `poll_until` itself (for
    /// recursive or mutually-referencing graphs).
    pub fn new(
        library: Arc<GraphLibrary>,
        script_engine: Arc<ScriptEngine>,
    ) -> (Self, PollUntilRegistrySlot) {
        let slot = Arc::new(std::sync::OnceLock::new());
        (
            Self {
                library,
                handlers: slot.clone(),
                exec_ctx: None,
                script_engine,
                predicate_asts: Arc::new(Mutex::new(HashMap::new())),
            },
            PollUntilRegistrySlot(slot),
        )
    }

    /// Bind a parent execution context so the inner subgraph invocation
    /// inherits the parent blackboard per its own `exec.context_inheritance`
    /// rules.
    pub fn with_context(mut self, ctx: Arc<ExecutionContext>) -> Self {
        self.exec_ctx = Some(ctx);
        self
    }
}

/// Deferred handler-registry initialisation slot mirroring
/// [`crate::handlers::subgraph_invoke::HandlerRegistrySlot`].
pub struct PollUntilRegistrySlot(Arc<std::sync::OnceLock<HandlerRegistry>>);

impl PollUntilRegistrySlot {
    pub fn set(self, registry: HandlerRegistry) {
        self.0.set(registry).ok();
    }
}

impl NodeHandler for PollUntilHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let config_json = node.config.clone();
        let node_id = node.id.0.clone();
        let library = self.library.clone();
        let handlers_lock = self.handlers.clone();
        let exec_ctx = self.exec_ctx.clone();
        let script_engine = self.script_engine.clone();
        let predicate_asts = self.predicate_asts.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before poll_until started".into(),
                });
            }

            // -- Load-time validation ---------------------------------------

            let cfg = PollUntilConfig::from_json(&config_json).map_err(|e| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': {e}"),
                recoverable: false,
            })?;

            // Subgraph must exist in the library (equivalent of "current
            // graph's subgraph table" in this codebase — the GraphLibrary is
            // the single source of truth for named subgraphs).
            if library.get(&cfg.graph_name).is_none() {
                return Err(NodeError::Failed {
                    source_message: None,
                    message: format!(
                        "node '{node_id}': config.graph '{}' not found in library",
                        cfg.graph_name
                    ),
                    recoverable: false,
                });
            }

            // Predicate must compile. Cached by source string.
            let predicate_ast = {
                let handler_cache = PredicateCache {
                    engine: &script_engine,
                    cache: &predicate_asts,
                };
                handler_cache
                    .get_or_compile(&cfg.predicate_src)
                    .map_err(|e| NodeError::Failed {
                        source_message: None,
                        message: format!("node '{node_id}': {e}"),
                        recoverable: false,
                    })?
            };

            // Handler registry must be initialised (required to recurse into
            // the subgraph).
            let handlers = handlers_lock.get().ok_or_else(|| NodeError::Failed {
                source_message: None,
                message: format!(
                    "node '{node_id}': handler registry not initialized for poll_until"
                ),
                recoverable: false,
            })?;

            // Build an inner subgraph handler per execute — it's a lightweight
            // value that borrows the shared library + handler registry clones.
            let mut inner =
                SubgraphInvocationHandler::with_handlers(library.clone(), handlers.clone());
            if let Some(ctx) = exec_ctx.as_ref() {
                inner = inner.with_context(ctx.clone());
            }

            // Synthesise the node the inner handler sees — same id so error
            // messages stay attributable, config carrying `graph`.
            let mut inner_node = Node::new(node_id.as_str(), node_id.as_str());
            inner_node.id = NodeId(node_id.clone());
            inner_node.handler = Some("_poll_until_inner".into());
            inner_node.config = serde_json::json!({ "graph": cfg.graph_name });

            // -- Attempt loop ----------------------------------------------

            let mut attempts_used: u32 = 0;
            let mut last_output: Outputs = Outputs::new();
            let mut predicate_matched = false;

            while attempts_used < cfg.max_attempts {
                // Leading delay only between attempts (not before the first).
                if attempts_used > 0 && cfg.delay_ms > 0 {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(cfg.delay_ms)) => {}
                        _ = cancel.cancelled() => {
                            return Err(NodeError::Cancelled {
                                reason: "cancelled during poll_until delay".into(),
                            });
                        }
                    }
                }

                // Invoke the subgraph — any failure propagates as a node failure.
                let invoke_fut = inner.execute(&inner_node, inputs.clone(), cancel.clone());
                let attempt_output = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return Err(NodeError::Cancelled {
                            reason: "cancelled during poll_until subgraph invocation".into(),
                        });
                    }
                    result = invoke_fut => result?,
                };

                attempts_used += 1;
                last_output = attempt_output;

                // Predicate evaluation — scope carries `output` (map of the
                // subgraph's final outputs) and `attempt` (1-indexed).
                let output_value = outputs_to_map_value(&last_output);
                let mut scope = Scope::new();
                scope.push_dynamic("output", value_to_dynamic(&output_value));
                scope.push_dynamic("attempt", Dynamic::from(attempts_used as i64));

                let eval_result = script_engine
                    .eval_ast(&mut scope, &predicate_ast, &cancel)
                    .map_err(|e| NodeError::Failed {
                        source_message: None,
                        message: format!(
                            "node '{node_id}': config.predicate evaluation failed on attempt {attempts_used}: {e}"
                        ),
                        recoverable: false,
                    })?;

                if eval_result.as_bool().unwrap_or(false) {
                    predicate_matched = true;
                    break;
                }
            }

            // -- Build outputs --------------------------------------------

            let timed_out = !predicate_matched;
            let mut outputs = Outputs::new();
            outputs.insert("attempts_used".into(), Value::I64(attempts_used as i64));
            outputs.insert("timed_out".into(), Value::Bool(timed_out));
            outputs.insert("output".into(), outputs_to_map_value(&last_output));
            Ok(outputs)
        })
    }

    fn validate_node(
        &self,
        node: &Node,
        _graph: &Graph,
        _ctx: &ExecutionContext,
    ) -> Result<(), Vec<ValidationIssue>> {
        let mut issues = Vec::new();

        // Shape first. Without a parsed config there's nothing further to
        // check.
        let cfg = match PollUntilConfig::from_json(&node.config) {
            Ok(c) => c,
            Err(e) => {
                issues.push(ValidationIssue::new(
                    String::new(),
                    String::new(),
                    ValidationIssueKind::Config,
                    e,
                ));
                return Err(issues);
            }
        };

        // Subgraph referenced by name must exist in the library.
        if self.library.get(&cfg.graph_name).is_none() {
            issues.push(ValidationIssue::new(
                String::new(),
                String::new(),
                ValidationIssueKind::MissingReference,
                format!("config.graph '{}' not found in library", cfg.graph_name),
            ));
        }

        // Predicate must compile. Uses the same cache the runtime path
        // reads so `execute()` doesn't recompile.
        let predicate_cache = PredicateCache {
            engine: &self.script_engine,
            cache: &self.predicate_asts,
        };
        if let Err(e) = predicate_cache.get_or_compile(&cfg.predicate_src) {
            issues.push(ValidationIssue::new(
                String::new(),
                String::new(),
                ValidationIssueKind::ScriptCompile,
                e,
            ));
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    fn schema(&self, name: &str) -> HandlerSchema {
        HandlerSchema::new(
            name,
            "Invoke a named subgraph in a fixed-delay attempt loop until a Rhai predicate over \
             the subgraph's output returns true, or max_attempts is hit (terminates with \
             timed_out=true, not a node failure).",
        )
        .with_config(
            SchemaField::new("graph", "string")
                .required()
                .describe("Name of the subgraph to invoke per attempt."),
        )
        .with_config(SchemaField::new("predicate", "string").required().describe(
            "Rhai expression over `output` (the subgraph output map) and `attempt` \
                     (1-indexed u32). Truthy return terminates the poll successfully.",
        ))
        .with_config(
            SchemaField::new("max_attempts", "integer")
                .required()
                .describe(
                    "Hard cap on attempts. On cap without the predicate matching, the \
                          node succeeds with timed_out=true.",
                ),
        )
        .with_config(SchemaField::new("delay_ms", "integer").required().describe(
            "Fixed delay in ms between attempts. The first attempt fires \
                          immediately with no leading delay.",
        ))
        .with_output(
            SchemaField::new("attempts_used", "integer")
                .describe("Number of subgraph invocations performed."),
        )
        .with_output(
            SchemaField::new("timed_out", "boolean")
                .describe("True iff max_attempts was reached without predicate satisfaction."),
        )
        .with_output(SchemaField::new("output", "map").describe(
            "Final subgraph output map (the one that matched the predicate, or the last \
                 one before the cap).",
        ))
    }
}

/// Helper to read-or-insert a compiled predicate AST. Separated from the
/// handler so it can be exercised without spinning up an execution.
struct PredicateCache<'a> {
    engine: &'a ScriptEngine,
    cache: &'a Mutex<HashMap<String, Arc<AST>>>,
}

impl<'a> PredicateCache<'a> {
    fn get_or_compile(&self, source: &str) -> Result<Arc<AST>, String> {
        if let Some(ast) = self.cache.lock().unwrap().get(source).cloned() {
            return Ok(ast);
        }
        let ast = self
            .engine
            .compile_expression(source)
            .map_err(|e| format!("config.predicate: {e}"))?;
        let arc = Arc::new(ast);
        self.cache
            .lock()
            .unwrap()
            .insert(source.to_string(), arc.clone());
        Ok(arc)
    }
}

/// Wrap a flat `Outputs` map as a single `Value::Map` for scripting consumers.
fn outputs_to_map_value(outputs: &Outputs) -> Value {
    let m: BTreeMap<String, Value> = outputs
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    Value::Map(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::sync_handler;
    use crate::graph::node::Node;
    use crate::graph::Graph;
    use crate::scripting::engine::default_script_engine;

    fn counter_graph() -> Graph {
        // INPUT (source, replaced by input injection) -> WORKER (sink).
        // The WORKER handler is what the outer `counter` name resolves to.
        let mut g = Graph::new();
        g.add_node(Node::new("INPUT", "Input").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("WORKER", "Worker").with_handler("counter"))
            .unwrap();
        g.add_edge(&"INPUT".into(), "", &"WORKER".into(), "", None)
            .unwrap();
        g
    }

    fn pass_handler_entry() -> (String, Arc<dyn NodeHandler>) {
        (
            "pass".into(),
            sync_handler(|_, inputs| Ok(inputs)) as Arc<dyn NodeHandler>,
        )
    }

    #[test]
    fn config_parses_happy_path() {
        let cfg = PollUntilConfig::from_json(&serde_json::json!({
            "graph": "g",
            "predicate": "output.done",
            "max_attempts": 5,
            "delay_ms": 10
        }))
        .unwrap();
        assert_eq!(cfg.graph_name, "g");
        assert_eq!(cfg.predicate_src, "output.done");
        assert_eq!(cfg.max_attempts, 5);
        assert_eq!(cfg.delay_ms, 10);
    }

    #[test]
    fn config_rejects_missing_keys() {
        let cases = [
            serde_json::json!({ "predicate": "true", "max_attempts": 1, "delay_ms": 0 }),
            serde_json::json!({ "graph": "g", "max_attempts": 1, "delay_ms": 0 }),
            serde_json::json!({ "graph": "g", "predicate": "true", "delay_ms": 0 }),
            serde_json::json!({ "graph": "g", "predicate": "true", "max_attempts": 1 }),
        ];
        for case in cases {
            let err = PollUntilConfig::from_json(&case).expect_err("should fail");
            assert!(err.contains("missing"), "unexpected error: {err}");
        }
    }

    #[test]
    fn config_rejects_zero_max_attempts() {
        let err = PollUntilConfig::from_json(&serde_json::json!({
            "graph": "g",
            "predicate": "true",
            "max_attempts": 0,
            "delay_ms": 0
        }))
        .expect_err("should fail");
        assert!(err.contains(">= 1"));
    }

    #[test]
    fn invalid_predicate_errors_at_handler_entry() {
        // Graph exists — should fail on predicate compile, not missing graph.
        let mut lib = GraphLibrary::new();
        lib.register("g", counter_graph());
        let handlers = HandlerRegistry::new();
        let engine = default_script_engine();
        let h = PollUntilHandler::with_handlers(Arc::new(lib), handlers, engine);

        let mut node = Node::new("P", "P");
        node.config = serde_json::json!({
            "graph": "g",
            "predicate": "let x = ;; garbage",
            "max_attempts": 1,
            "delay_ms": 0,
        });

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(h.execute(&node, Outputs::new(), CancellationToken::new()))
            .expect_err("should fail");
        assert!(err.to_string().contains("config.predicate"));
    }

    #[test]
    fn missing_graph_errors_at_handler_entry() {
        let lib = GraphLibrary::new();
        let handlers = HandlerRegistry::new();
        let engine = default_script_engine();
        let h = PollUntilHandler::with_handlers(Arc::new(lib), handlers, engine);

        let mut node = Node::new("P", "P");
        node.config = serde_json::json!({
            "graph": "missing",
            "predicate": "true",
            "max_attempts": 1,
            "delay_ms": 0,
        });

        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(h.execute(&node, Outputs::new(), CancellationToken::new()))
            .expect_err("should fail");
        assert!(err.to_string().contains("not found in library"));
    }

    #[test]
    fn validate_node_flags_missing_subgraph() {
        let lib = GraphLibrary::new();
        let engine = default_script_engine();
        let h = PollUntilHandler::with_handlers(Arc::new(lib), HandlerRegistry::new(), engine);

        let mut node = Node::new("P", "P");
        node.config = serde_json::json!({
            "graph": "nope",
            "predicate": "true",
            "max_attempts": 1,
            "delay_ms": 0,
        });
        let graph = Graph::new();
        let ctx = ExecutionContext::new();
        let issues = h.validate_node(&node, &graph, &ctx).unwrap_err();
        assert!(issues
            .iter()
            .any(|i| matches!(i.kind, ValidationIssueKind::MissingReference)));
    }

    #[test]
    fn validate_node_flags_bad_predicate_and_missing_subgraph() {
        // Collect-all behaviour: both issues reported together.
        let lib = GraphLibrary::new();
        let engine = default_script_engine();
        let h = PollUntilHandler::with_handlers(Arc::new(lib), HandlerRegistry::new(), engine);

        let mut node = Node::new("P", "P");
        node.config = serde_json::json!({
            "graph": "nope",
            "predicate": "let x = ;; garbage",
            "max_attempts": 1,
            "delay_ms": 0,
        });
        let graph = Graph::new();
        let ctx = ExecutionContext::new();
        let issues = h.validate_node(&node, &graph, &ctx).unwrap_err();
        assert_eq!(issues.len(), 2);
    }

    #[test]
    fn validate_node_caches_predicate_for_runtime() {
        let mut lib = GraphLibrary::new();
        lib.register("g", counter_graph());
        let engine = default_script_engine();
        let h = PollUntilHandler::with_handlers(Arc::new(lib), HandlerRegistry::new(), engine);

        let mut node = Node::new("P", "P");
        node.config = serde_json::json!({
            "graph": "g",
            "predicate": "output.done",
            "max_attempts": 1,
            "delay_ms": 0,
        });
        let graph = Graph::new();
        let ctx = ExecutionContext::new();
        h.validate_node(&node, &graph, &ctx).unwrap();
        assert_eq!(h.predicate_asts.lock().unwrap().len(), 1);
    }

    #[test]
    fn predicate_compile_is_cached() {
        // Directly exercise the cache rather than rely on the handler path —
        // compile_expression is deterministic, so calling twice with the same
        // source and asserting the cache size stays at 1 is sufficient.
        let engine = default_script_engine();
        let cache: Mutex<HashMap<String, Arc<AST>>> = Mutex::new(HashMap::new());
        let c = PredicateCache {
            engine: &engine,
            cache: &cache,
        };
        let a = c.get_or_compile("output.done").unwrap();
        let b = c.get_or_compile("output.done").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(cache.lock().unwrap().len(), 1);
    }

    // A minimal end-to-end happy-path test that doesn't spin up a runtime
    // inside #[test] — it lives in the integration suite. We also check the
    // empty-handlers path here: an uninitialised deferred slot must error.

    #[tokio::test]
    async fn uninitialized_registry_errors() {
        let mut lib = GraphLibrary::new();
        lib.register("g", counter_graph());
        let engine = default_script_engine();
        let (h, _slot) = PollUntilHandler::new(Arc::new(lib), engine);
        // Do not call slot.set()

        let mut node = Node::new("P", "P");
        node.config = serde_json::json!({
            "graph": "g",
            "predicate": "true",
            "max_attempts": 1,
            "delay_ms": 0,
        });

        let err = h
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .expect_err("should fail");
        assert!(err.to_string().contains("not initialized"));
    }

    #[tokio::test]
    async fn first_attempt_match_no_delay_fired() {
        // Subgraph "g" is a single node whose handler always returns { done: true }.
        let mut lib = GraphLibrary::new();
        lib.register("g", counter_graph());
        let mut handlers = HandlerRegistry::new();
        let (pname, ph) = pass_handler_entry();
        handlers.insert(pname, ph);
        handlers.insert(
            "counter".into(),
            sync_handler(|_, _| {
                let mut out = Outputs::new();
                out.insert("done".into(), Value::Bool(true));
                Ok(out)
            }),
        );
        let engine = default_script_engine();
        let h = PollUntilHandler::with_handlers(Arc::new(lib), handlers, engine);

        let mut node = Node::new("P", "P");
        node.config = serde_json::json!({
            "graph": "g",
            "predicate": "output.done == true",
            "max_attempts": 5,
            "delay_ms": 500,
        });

        let start = std::time::Instant::now();
        let outputs = h
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        let elapsed = start.elapsed();

        assert_eq!(outputs.get("attempts_used"), Some(&Value::I64(1)));
        assert_eq!(outputs.get("timed_out"), Some(&Value::Bool(false)));
        assert!(
            elapsed < Duration::from_millis(300),
            "no delay should fire before first attempt; elapsed={elapsed:?}"
        );
    }
}
