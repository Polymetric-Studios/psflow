//! `loop` handler — accumulating loop over a subgraph (generalizes `poll_until`).
//!
//! Each iteration invokes a named subgraph, appends its produced items to a
//! growing (optionally deduped) collection, and re-evaluates termination. The
//! accumulated collection is injected back into the next iteration's inputs as
//! `state`, so a subgraph can "find what it hasn't found yet."
//!
//! Termination (first to fire wins):
//! - `until` — a Rhai expression over `state` (accumulated list), `iteration`
//!   (1-indexed), and `output` (last round's output map). Truthy → stop. Covers
//!   loop-until-count (`len(state) >= 10`) and loop-until-condition.
//! - `until_dry` — stop after this many consecutive rounds that add no new
//!   items. Covers loop-until-dry.
//! - `max_iterations` — hard cap (required backstop).
//!
//! Composes the same machinery as `poll_until`: `SubgraphInvocationHandler`
//! (per-iteration invocation), the script engine (predicate/dedup compilation),
//! and the deferred handler-registry slot.

use crate::error::NodeError;
use crate::execute::{
    CancellationToken, ExecutionContext, HandlerRegistry, HandlerSchema, NodeHandler, Outputs,
    SchemaField,
};
use crate::graph::node::{Node, NodeId};
use crate::graph::types::Value;
use crate::handlers::subgraph_invoke::{GraphLibrary, SubgraphInvocationHandler};
use crate::scripting::bridge::value_to_dynamic;
use crate::scripting::engine::ScriptEngine;
use rhai::{Dynamic, Scope, AST};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const LOOP_HANDLER_NAME: &str = "loop";
const DEFAULT_STATE_INPUT: &str = "state";

struct LoopConfig {
    graph_name: String,
    collect: Option<String>,
    until_src: Option<String>,
    until_dry: Option<u32>,
    dedup_src: Option<String>,
    max_iterations: u32,
    delay_ms: u64,
    state_input: String,
}

impl LoopConfig {
    fn from_json(config: &serde_json::Value) -> Result<Self, String> {
        let graph_name = config
            .get("graph")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing config.graph (string)".to_string())?
            .to_string();
        let max_iterations = config
            .get("max_iterations")
            .and_then(|v| v.as_u64())
            .filter(|n| *n >= 1 && *n <= u32::MAX as u64)
            .ok_or_else(|| "config.max_iterations must be an integer >= 1".to_string())?
            as u32;
        let until_dry = config
            .get("until_dry")
            .and_then(|v| v.as_u64())
            .map(|v| v.min(u32::MAX as u64) as u32);
        Ok(Self {
            graph_name,
            collect: config
                .get("collect")
                .and_then(|v| v.as_str())
                .map(String::from),
            until_src: config
                .get("until")
                .and_then(|v| v.as_str())
                .map(String::from),
            until_dry,
            dedup_src: config
                .get("dedup_key")
                .and_then(|v| v.as_str())
                .map(String::from),
            max_iterations,
            delay_ms: config.get("delay_ms").and_then(|v| v.as_u64()).unwrap_or(0),
            state_input: config
                .get("state_as")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_STATE_INPUT)
                .to_string(),
        })
    }
}

pub struct LoopHandler {
    library: Arc<GraphLibrary>,
    handlers: Arc<OnceLock<HandlerRegistry>>,
    exec_ctx: Option<Arc<ExecutionContext>>,
    script_engine: Arc<ScriptEngine>,
    asts: Arc<Mutex<HashMap<String, Arc<AST>>>>,
}

impl LoopHandler {
    pub fn with_handlers(
        library: Arc<GraphLibrary>,
        handlers: HandlerRegistry,
        script_engine: Arc<ScriptEngine>,
    ) -> Self {
        let slot = Arc::new(OnceLock::new());
        slot.set(handlers).ok();
        Self {
            library,
            handlers: slot,
            exec_ctx: None,
            script_engine,
            asts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn new(
        library: Arc<GraphLibrary>,
        script_engine: Arc<ScriptEngine>,
    ) -> (Self, LoopRegistrySlot) {
        let slot = Arc::new(OnceLock::new());
        (
            Self {
                library,
                handlers: slot.clone(),
                exec_ctx: None,
                script_engine,
                asts: Arc::new(Mutex::new(HashMap::new())),
            },
            LoopRegistrySlot(slot),
        )
    }

    pub fn with_context(mut self, ctx: Arc<ExecutionContext>) -> Self {
        self.exec_ctx = Some(ctx);
        self
    }

    fn compile(&self, src: &str) -> Result<Arc<AST>, String> {
        if let Some(ast) = self.asts.lock().unwrap().get(src).cloned() {
            return Ok(ast);
        }
        let ast = Arc::new(
            self.script_engine
                .compile_expression(src)
                .map_err(|e| format!("script compile error: {e}"))?,
        );
        self.asts
            .lock()
            .unwrap()
            .insert(src.to_string(), ast.clone());
        Ok(ast)
    }
}

pub struct LoopRegistrySlot(Arc<OnceLock<HandlerRegistry>>);

impl LoopRegistrySlot {
    pub fn set(self, registry: HandlerRegistry) {
        self.0.set(registry).ok();
    }
}

fn outputs_to_map_value(outputs: &Outputs) -> Value {
    Value::Map(
        outputs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    )
}

/// Stable string key for dedup when no `dedup_key` expression is given.
fn canonical_key(v: &Value) -> String {
    serde_json::Value::from(v).to_string()
}

impl NodeHandler for LoopHandler {
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

        // Pre-compile predicates (so compile errors surface before the loop).
        let cfg = match LoopConfig::from_json(&config_json) {
            Ok(c) => c,
            Err(e) => {
                return Box::pin(async move {
                    Err(NodeError::Failed {
                        source_message: None,
                        message: format!("node '{node_id}': {e}"),
                        recoverable: false,
                    })
                })
            }
        };
        let until_ast = match cfg
            .until_src
            .as_deref()
            .map(|s| self.compile(s))
            .transpose()
        {
            Ok(a) => a,
            Err(e) => {
                return Box::pin(async move {
                    Err(NodeError::Failed {
                        source_message: None,
                        message: format!("node '{node_id}': config.until {e}"),
                        recoverable: false,
                    })
                })
            }
        };
        let dedup_ast = match cfg
            .dedup_src
            .as_deref()
            .map(|s| self.compile(s))
            .transpose()
        {
            Ok(a) => a,
            Err(e) => {
                return Box::pin(async move {
                    Err(NodeError::Failed {
                        source_message: None,
                        message: format!("node '{node_id}': config.dedup_key {e}"),
                        recoverable: false,
                    })
                })
            }
        };

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before loop".into(),
                });
            }
            let fail = |msg: String| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': {msg}"),
                recoverable: false,
            };

            if library.get(&cfg.graph_name).is_none() {
                return Err(fail(format!(
                    "config.graph '{}' not found in library",
                    cfg.graph_name
                )));
            }
            let handlers = handlers_lock
                .get()
                .ok_or_else(|| fail("handler registry not initialized for loop".into()))?;

            let mut inner =
                SubgraphInvocationHandler::with_handlers(library.clone(), handlers.clone());
            if let Some(ctx) = exec_ctx.as_ref() {
                inner = inner.with_context(ctx.clone());
            }
            let mut inner_node = Node::new(node_id.as_str(), node_id.as_str());
            inner_node.id = NodeId(node_id.clone());
            inner_node.config = serde_json::json!({ "graph": cfg.graph_name });

            let mut collected: Vec<Value> = Vec::new();
            let mut seen: HashSet<String> = HashSet::new();
            let mut iterations: u32 = 0;
            let mut dry_rounds: u32 = 0;
            let mut last_output = Outputs::new();
            let mut stopped_by = "max_iterations";

            while iterations < cfg.max_iterations {
                if iterations > 0 && cfg.delay_ms > 0 {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(cfg.delay_ms)) => {}
                        _ = cancel.cancelled() => {
                            return Err(NodeError::Cancelled { reason: "cancelled during loop delay".into() });
                        }
                    }
                }

                // Inject the accumulated state so the subgraph can use it.
                let mut iter_inputs = inputs.clone();
                iter_inputs.insert(cfg.state_input.clone(), Value::Vec(collected.clone()));

                let output = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return Err(NodeError::Cancelled { reason: "cancelled during loop iteration".into() });
                    }
                    res = inner.execute(&inner_node, iter_inputs, cancel.clone()) => res?,
                };
                iterations += 1;
                last_output = output.clone();

                // Items this round: the `collect` list output, or the whole
                // output map as a single item when `collect` is unset.
                let items: Vec<Value> = match &cfg.collect {
                    Some(key) => match output.get(key) {
                        Some(Value::Vec(v)) => v.clone(),
                        Some(other) => vec![other.clone()],
                        None => Vec::new(),
                    },
                    None => vec![outputs_to_map_value(&output)],
                };

                let mut new_items = 0u32;
                for item in items {
                    let key = match &dedup_ast {
                        Some(ast) => {
                            let mut scope = Scope::new();
                            scope.push_dynamic("item", value_to_dynamic(&item));
                            match script_engine.eval_ast(&mut scope, ast, &cancel) {
                                Ok(d) => d.to_string(),
                                Err(e) => {
                                    return Err(fail(format!("config.dedup_key eval failed: {e}")))
                                }
                            }
                        }
                        None => canonical_key(&item),
                    };
                    if seen.insert(key) {
                        collected.push(item);
                        new_items += 1;
                    }
                }
                if new_items == 0 {
                    dry_rounds += 1;
                } else {
                    dry_rounds = 0;
                }

                if let Some(k) = cfg.until_dry {
                    if dry_rounds >= k {
                        stopped_by = "until_dry";
                        break;
                    }
                }
                if let Some(ast) = &until_ast {
                    let mut scope = Scope::new();
                    scope.push_dynamic("state", value_to_dynamic(&Value::Vec(collected.clone())));
                    scope.push_dynamic("iteration", Dynamic::from(iterations as i64));
                    scope.push_dynamic(
                        "output",
                        value_to_dynamic(&outputs_to_map_value(&last_output)),
                    );
                    let matched = script_engine
                        .eval_ast(&mut scope, ast, &cancel)
                        .map_err(|e| {
                            fail(format!(
                                "config.until eval failed on iteration {iterations}: {e}"
                            ))
                        })?
                        .as_bool()
                        .unwrap_or(false);
                    if matched {
                        stopped_by = "until";
                        break;
                    }
                }
            }

            let mut outputs = Outputs::new();
            outputs.insert("collected".into(), Value::Vec(collected.clone()));
            outputs.insert("count".into(), Value::I64(collected.len() as i64));
            outputs.insert("iterations".into(), Value::I64(iterations as i64));
            outputs.insert("dry_rounds".into(), Value::I64(dry_rounds as i64));
            outputs.insert("stopped_by".into(), Value::String(stopped_by.into()));
            outputs.insert("output".into(), outputs_to_map_value(&last_output));
            Ok(outputs)
        })
    }

    fn schema(&self, name: &str) -> HandlerSchema {
        HandlerSchema::new(
            name,
            "Loop a subgraph, accumulating its produced items until a condition, dry rounds, or a cap",
        )
        .with_config(SchemaField::new("graph", "string").required().describe("Subgraph invoked per iteration"))
        .with_config(SchemaField::new("collect", "string").describe("Output key holding the per-round list of items (default: accumulate the whole output map per round)"))
        .with_config(SchemaField::new("until", "string").describe("Rhai over state/iteration/output; truthy stops"))
        .with_config(SchemaField::new("until_dry", "integer").describe("Stop after N consecutive rounds adding no new items"))
        .with_config(SchemaField::new("dedup_key", "string").describe("Rhai over `item` returning a dedup key"))
        .with_config(SchemaField::new("max_iterations", "integer").required().describe("Hard cap (>= 1)"))
        .with_config(SchemaField::new("delay_ms", "integer").describe("Delay between iterations").default(serde_json::json!(0)))
        .with_config(SchemaField::new("state_as", "string").describe("Input key the accumulated list is injected as").default(serde_json::json!(DEFAULT_STATE_INPUT)))
        .with_output(SchemaField::new("collected", "array"))
        .with_output(SchemaField::new("count", "integer"))
        .with_output(SchemaField::new("iterations", "integer"))
        .with_output(SchemaField::new("dry_rounds", "integer"))
        .with_output(SchemaField::new("stopped_by", "string"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::sync_handler;
    use crate::graph::Graph;
    use crate::scripting::engine::default_script_engine;

    /// Subgraph: SRC (injected) -> GEN(handler).
    fn gen_graph(handler: &str) -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::new("SRC", "Src").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("GEN", "Gen").with_handler(handler))
            .unwrap();
        g.add_edge(&"SRC".into(), "", &"GEN".into(), "", None)
            .unwrap();
        g
    }

    fn handlers() -> HandlerRegistry {
        let mut h = HandlerRegistry::new();
        h.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        // Returns [len(state), len(state)+1] — grows by 2 unique items each round.
        h.insert(
            "grow".into(),
            sync_handler(|_, inputs| {
                let n = match inputs.get("state") {
                    Some(Value::Vec(v)) => v.len() as i64,
                    _ => 0,
                };
                let mut o = Outputs::new();
                o.insert(
                    "items".into(),
                    Value::Vec(vec![Value::I64(n), Value::I64(n + 1)]),
                );
                Ok(o)
            }),
        );
        // Always returns the same two items — dups after round 1.
        h.insert(
            "fixed".into(),
            sync_handler(|_, _| {
                let mut o = Outputs::new();
                o.insert(
                    "items".into(),
                    Value::Vec(vec![Value::I64(10), Value::I64(11)]),
                );
                Ok(o)
            }),
        );
        h
    }

    fn loop_node(cfg: serde_json::Value) -> Node {
        let mut n = Node::new("LOOP", "Loop");
        n.config = cfg;
        n
    }

    fn run(h: &LoopHandler, node: &Node) -> Outputs {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(h.execute(node, Outputs::new(), CancellationToken::new()))
            .unwrap()
    }

    #[test]
    fn until_count_accumulates() {
        let mut lib = GraphLibrary::new();
        lib.register("gen", gen_graph("grow"));
        let h = LoopHandler::with_handlers(Arc::new(lib), handlers(), default_script_engine());

        let out = run(
            &h,
            &loop_node(serde_json::json!({
                "graph": "gen", "collect": "items",
                "until": "len(state) >= 5", "max_iterations": 10
            })),
        );
        // rounds: [0,1] -> [0,1,2,3] -> [0,1,2,3,4,5] (len 6 >= 5)
        assert_eq!(out.get("iterations"), Some(&Value::I64(3)));
        assert_eq!(out.get("count"), Some(&Value::I64(6)));
        assert_eq!(out.get("stopped_by"), Some(&Value::String("until".into())));
    }

    #[test]
    fn until_dry_stops() {
        let mut lib = GraphLibrary::new();
        lib.register("gen", gen_graph("fixed"));
        let h = LoopHandler::with_handlers(Arc::new(lib), handlers(), default_script_engine());

        let out = run(
            &h,
            &loop_node(serde_json::json!({
                "graph": "gen", "collect": "items",
                "until_dry": 2, "max_iterations": 10
            })),
        );
        // round1: 2 new; round2: 0 new (dry 1); round3: 0 new (dry 2) -> stop
        assert_eq!(out.get("iterations"), Some(&Value::I64(3)));
        assert_eq!(out.get("count"), Some(&Value::I64(2)));
        assert_eq!(out.get("dry_rounds"), Some(&Value::I64(2)));
        assert_eq!(
            out.get("stopped_by"),
            Some(&Value::String("until_dry".into()))
        );
    }

    #[test]
    fn max_iterations_caps() {
        let mut lib = GraphLibrary::new();
        lib.register("gen", gen_graph("grow"));
        let h = LoopHandler::with_handlers(Arc::new(lib), handlers(), default_script_engine());

        let out = run(
            &h,
            &loop_node(serde_json::json!({
                "graph": "gen", "collect": "items", "max_iterations": 2
            })),
        );
        assert_eq!(out.get("iterations"), Some(&Value::I64(2)));
        assert_eq!(out.get("count"), Some(&Value::I64(4))); // [0,1,2,3]
        assert_eq!(
            out.get("stopped_by"),
            Some(&Value::String("max_iterations".into()))
        );
    }

    #[test]
    fn missing_graph_errors() {
        let h = LoopHandler::with_handlers(
            Arc::new(GraphLibrary::new()),
            HandlerRegistry::new(),
            default_script_engine(),
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(h.execute(
                &loop_node(serde_json::json!({ "graph": "nope", "max_iterations": 1 })),
                Outputs::new(),
                CancellationToken::new(),
            ))
            .unwrap_err();
        assert!(err.to_string().contains("not found in library"));
    }

    #[test]
    fn bad_config_errors() {
        let h = LoopHandler::with_handlers(
            Arc::new(GraphLibrary::new()),
            HandlerRegistry::new(),
            default_script_engine(),
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let err = rt
            .block_on(h.execute(
                &loop_node(serde_json::json!({ "graph": "g" })), // missing max_iterations
                Outputs::new(),
                CancellationToken::new(),
            ))
            .unwrap_err();
        assert!(err.to_string().contains("max_iterations"));
    }
}
