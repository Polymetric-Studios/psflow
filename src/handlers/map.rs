//! `map` handler — data-driven fan-out over a runtime list.
//!
//! Where `parallel:` fans out a static, author-time set of nodes, `map` fans out
//! over a list whose length is only known at runtime: it invokes a named
//! subgraph once per element (concurrently, capped), then reduces the per-item
//! results. This is psflow's answer to the imperative `items.map(run)` pattern,
//! while keeping the graph declarative.
//!
//! It composes existing machinery: [`GraphLibrary`] + the deferred handler
//! registry, `execute_child` (per-element invocation + input injection), the
//! `DepthGuard` recursion guard, and context inheritance.

use crate::error::NodeError;
use crate::execute::blackboard::{Blackboard, ContextInheritance};
use crate::execute::context::CancellationToken;
use crate::execute::{
    ExecutionContext, HandlerRegistry, HandlerSchema, NodeHandler, NodeState, Outputs, SchemaField,
    TopologicalExecutor,
};
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::graph::Graph;
use crate::handlers::subgraph_invoke::{
    execute_child, DepthGuard, GraphLibrary, HandlerRegistrySlot,
};
use futures::stream::{self, StreamExt};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, OnceLock};

const DEFAULT_MAX_DEPTH: usize = 10;
const DEFAULT_MAX_CONCURRENCY: usize = 16;
const REDUCE_COLLECT: &str = "collect";
const REDUCE_QUORUM: &str = "quorum";
const ON_ERROR_FAIL: &str = "fail";

/// Fans out a named subgraph over a runtime list.
///
/// ## Configuration
///
/// - `config.over` (required): input key whose value is the list to map over
///   (a `Value::Vec`). Each element drives one subgraph invocation.
/// - `config.graph` (required): name of the subgraph in the library.
/// - `config.as`: input key the element is bound to for the subgraph (default `item`).
/// - `config.max_concurrency`: max concurrent invocations (default 16).
/// - `config.reduce`: `collect` (default) → `results` (Vec of per-item output
///   maps) + `count`; or `quorum` → `votes`/`passed` over a boolean field.
/// - `config.quorum.field` / `config.quorum.threshold`: for `reduce: quorum`.
/// - `config.on_item_error`: `skip` (default — failed items omitted, counted in
///   `errors`) or `fail` (any item failure fails the node).
/// - `exec.max_depth`: nesting guard for the map node (default 10).
/// - `exec.context_inheritance`: `read_only` (default) | `snapshot` | `isolated`.
///
/// ## Outputs
///
/// - `collect`: `results` (Vec), `count` (I64), `errors` (I64).
/// - `quorum`: `votes` (I64), `passed` (Bool), `count` (I64), `errors` (I64).
pub struct MapHandler {
    library: Arc<GraphLibrary>,
    handlers: Arc<OnceLock<HandlerRegistry>>,
    exec_ctx: Option<Arc<ExecutionContext>>,
    active_depth: Arc<AtomicUsize>,
    default_max_depth: usize,
}

impl MapHandler {
    /// Create a handler with a deferred registry slot (set before first use).
    pub fn new(library: Arc<GraphLibrary>) -> (Self, HandlerRegistrySlot) {
        let slot = Arc::new(OnceLock::new());
        let handler = Self {
            library,
            handlers: slot.clone(),
            exec_ctx: None,
            active_depth: Arc::new(AtomicUsize::new(0)),
            default_max_depth: DEFAULT_MAX_DEPTH,
        };
        (handler, HandlerRegistrySlot::from_slot(slot))
    }

    /// Create a handler with a pre-set registry.
    pub fn with_handlers(library: Arc<GraphLibrary>, handlers: HandlerRegistry) -> Self {
        let slot = Arc::new(OnceLock::new());
        slot.set(handlers).ok();
        Self {
            library,
            handlers: slot,
            exec_ctx: None,
            active_depth: Arc::new(AtomicUsize::new(0)),
            default_max_depth: DEFAULT_MAX_DEPTH,
        }
    }

    /// Set the parent execution context for blackboard inheritance.
    pub fn with_context(mut self, ctx: Arc<ExecutionContext>) -> Self {
        self.exec_ctx = Some(ctx);
        self
    }
}

fn outputs_to_value(outputs: Outputs) -> Value {
    Value::Map(outputs.into_iter().collect::<BTreeMap<_, _>>())
}

impl NodeHandler for MapHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let library = self.library.clone();
        let handlers_lock = self.handlers.clone();
        let active_depth = self.active_depth.clone();
        let exec_ctx = self.exec_ctx.clone();
        let config = node.config.clone();
        let exec = node.exec.clone();
        let node_id = node.id.0.clone();
        let default_max_depth = self.default_max_depth;

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before map".into(),
                });
            }

            let fail = |msg: String| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': {msg}"),
                recoverable: false,
            };

            let over_key = config
                .get("over")
                .and_then(|v| v.as_str())
                .ok_or_else(|| fail("missing config.over (the input list key)".into()))?
                .to_string();
            let graph_name = config
                .get("graph")
                .and_then(|v| v.as_str())
                .ok_or_else(|| fail("missing config.graph".into()))?
                .to_string();
            let as_key = config
                .get("as")
                .and_then(|v| v.as_str())
                .unwrap_or("item")
                .to_string();
            let max_concurrency = config
                .get("max_concurrency")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .filter(|n| *n > 0)
                .unwrap_or(DEFAULT_MAX_CONCURRENCY);
            let reduce = config
                .get("reduce")
                .and_then(|v| v.as_str())
                .unwrap_or(REDUCE_COLLECT)
                .to_string();
            let fail_on_item_error = config
                .get("on_item_error")
                .and_then(|v| v.as_str())
                .map(|s| s == ON_ERROR_FAIL)
                .unwrap_or(false);
            let max_depth = exec
                .get("max_depth")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(default_max_depth);
            let inheritance = match exec.get("context_inheritance").and_then(|v| v.as_str()) {
                Some("snapshot") => ContextInheritance::Snapshot,
                Some("isolated") => ContextInheritance::Isolated,
                _ => ContextInheritance::ReadOnly,
            };

            // The map node itself is one nesting level (breadth is bounded by
            // max_concurrency, not the depth guard).
            let _guard = DepthGuard::enter(&active_depth, max_depth)?;

            let items: Vec<Value> = match inputs.get(&over_key) {
                Some(Value::Vec(v)) => v.clone(),
                Some(_) => return Err(fail(format!("input '{over_key}' is not a list"))),
                None => return Err(fail(format!("missing input '{over_key}' to map over"))),
            };

            let child_graph: Arc<Graph> = Arc::new(
                library
                    .get(&graph_name)
                    .ok_or_else(|| fail(format!("graph '{graph_name}' not found in library")))?
                    .clone(),
            );
            let handlers: Arc<HandlerRegistry> = Arc::new(
                handlers_lock
                    .get()
                    .ok_or_else(|| fail("handler registry not initialized".into()))?
                    .clone(),
            );

            let source_nodes: Arc<Vec<String>> = Arc::new(
                child_graph
                    .nodes()
                    .filter(|n| child_graph.predecessors(&n.id).is_empty())
                    .map(|n| n.id.0.clone())
                    .collect(),
            );
            let sink_nodes: Arc<Vec<String>> = Arc::new(
                child_graph
                    .nodes()
                    .filter(|n| child_graph.successors(&n.id).is_empty())
                    .map(|n| n.id.0.clone())
                    .collect(),
            );
            let parent_bb: Arc<Option<Blackboard>> =
                Arc::new(exec_ctx.as_ref().map(|c| c.blackboard().clone()));
            let base_inputs = Arc::new(inputs);
            let as_key = Arc::new(as_key);

            // Fan out: one subgraph invocation per element, order preserved,
            // concurrency capped. Each item gets a fresh executor.
            let item_results: Vec<Result<Outputs, String>> =
                stream::iter(items.into_iter().enumerate())
                    .map(|(idx, item)| {
                        let child_graph = child_graph.clone();
                        let handlers = handlers.clone();
                        let source_nodes = source_nodes.clone();
                        let sink_nodes = sink_nodes.clone();
                        let parent_bb = parent_bb.clone();
                        let base_inputs = base_inputs.clone();
                        let as_key = as_key.clone();
                        let cancel = cancel.clone();
                        async move {
                            let mut child_inputs = (*base_inputs).clone();
                            child_inputs.insert((*as_key).clone(), item);
                            let executor = TopologicalExecutor::with_cancel(cancel);
                            let res = execute_child(
                                &child_graph,
                                &handlers,
                                &executor,
                                &child_inputs,
                                &source_nodes,
                                (*parent_bb).as_ref(),
                                inheritance,
                            )
                            .await
                            .map_err(|e| format!("item {idx}: {e}"))?;

                            if res.node_states.values().any(|s| *s == NodeState::Failed) {
                                return Err(format!("item {idx}: a node failed"));
                            }
                            let mut out = Outputs::new();
                            for sink in sink_nodes.iter() {
                                if let Some(o) = res.node_outputs.get(sink) {
                                    out.extend(o.clone());
                                }
                            }
                            Ok(out)
                        }
                    })
                    .buffered(max_concurrency)
                    .collect()
                    .await;

            let errors = item_results.iter().filter(|r| r.is_err()).count();
            if fail_on_item_error {
                if let Some(Err(e)) = item_results.iter().find(|r| r.is_err()) {
                    return Err(fail(e.clone()));
                }
            }
            let oks: Vec<Outputs> = item_results.into_iter().flatten().collect();

            let mut outputs = Outputs::new();
            outputs.insert("count".into(), Value::I64(oks.len() as i64));
            outputs.insert("errors".into(), Value::I64(errors as i64));

            match reduce.as_str() {
                REDUCE_QUORUM => {
                    let field = config
                        .get("quorum")
                        .and_then(|q| q.get("field"))
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            fail("reduce: quorum requires config.quorum.field".into())
                        })?;
                    let threshold = config
                        .get("quorum")
                        .and_then(|q| q.get("threshold"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(1);
                    let votes = oks
                        .iter()
                        .filter(|o| matches!(o.get(field), Some(Value::Bool(true))))
                        .count() as i64;
                    outputs.insert("votes".into(), Value::I64(votes));
                    outputs.insert("passed".into(), Value::Bool(votes >= threshold));
                }
                _ => {
                    let results: Vec<Value> = oks.into_iter().map(outputs_to_value).collect();
                    outputs.insert("results".into(), Value::Vec(results));
                }
            }

            Ok(outputs)
        })
    }

    fn schema(&self, name: &str) -> HandlerSchema {
        HandlerSchema::new(name, "Fan out a subgraph over a runtime list and reduce")
            .with_config(
                SchemaField::new("over", "string")
                    .required()
                    .describe("Input key whose value is the list to map over"),
            )
            .with_config(
                SchemaField::new("graph", "string")
                    .required()
                    .describe("Subgraph (library) name run once per element"),
            )
            .with_config(
                SchemaField::new("as", "string")
                    .describe("Input key the element is bound to")
                    .default(serde_json::json!("item")),
            )
            .with_config(
                SchemaField::new("max_concurrency", "integer")
                    .describe("Max concurrent invocations")
                    .default(serde_json::json!(DEFAULT_MAX_CONCURRENCY)),
            )
            .with_config(
                SchemaField::new("reduce", "string")
                    .describe("collect | quorum")
                    .default(serde_json::json!(REDUCE_COLLECT)),
            )
            .with_config(
                SchemaField::new("on_item_error", "string")
                    .describe("skip | fail")
                    .default(serde_json::json!("skip")),
            )
            .with_output(SchemaField::new("results", "array"))
            .with_output(SchemaField::new("count", "integer"))
            .with_output(SchemaField::new("errors", "integer"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::sync_handler;

    /// Child graph: SRC (injected with parent inputs incl. `item`) -> WORK.
    /// WORK reads `inputs.item` and runs `handler`.
    fn child_with(work_handler: &str) -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::new("SRC", "Src").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("WORK", "Work").with_handler(work_handler))
            .unwrap();
        g.add_edge(&"SRC".into(), "", &"WORK".into(), "", None)
            .unwrap();
        g
    }

    fn item_of(inputs: &Outputs) -> i64 {
        match inputs.get("item") {
            Some(Value::I64(n)) => *n,
            _ => 0,
        }
    }

    fn handlers() -> HandlerRegistry {
        let mut h = HandlerRegistry::new();
        h.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        h.insert(
            "double".into(),
            sync_handler(|_, inputs| {
                let mut o = Outputs::new();
                o.insert("doubled".into(), Value::I64(item_of(&inputs) * 2));
                Ok(o)
            }),
        );
        h.insert(
            "is_big".into(),
            sync_handler(|_, inputs| {
                let mut o = Outputs::new();
                o.insert("real".into(), Value::Bool(item_of(&inputs) >= 2));
                Ok(o)
            }),
        );
        h.insert(
            "fail_on_two".into(),
            sync_handler(|_, inputs| {
                if item_of(&inputs) == 2 {
                    return Err(NodeError::Failed {
                        source_message: None,
                        message: "boom on 2".into(),
                        recoverable: false,
                    });
                }
                let mut o = Outputs::new();
                o.insert("ok".into(), Value::I64(item_of(&inputs)));
                Ok(o)
            }),
        );
        h
    }

    fn map_node(graph: &str, reduce: serde_json::Value) -> Node {
        let mut n = Node::new("MAP", "Map");
        n.config = serde_json::json!({ "over": "nums", "graph": graph, "as": "item" });
        if let serde_json::Value::Object(extra) = reduce {
            if let serde_json::Value::Object(cfg) = &mut n.config {
                cfg.extend(extra);
            }
        }
        n
    }

    fn nums(list: &[i64]) -> Outputs {
        let mut i = Outputs::new();
        i.insert(
            "nums".into(),
            Value::Vec(list.iter().map(|n| Value::I64(*n)).collect()),
        );
        i
    }

    #[tokio::test]
    async fn map_collect_preserves_order() {
        let mut lib = GraphLibrary::new();
        lib.register("dbl", child_with("double"));
        let h = MapHandler::with_handlers(Arc::new(lib), handlers());

        let out = h
            .execute(
                &map_node("dbl", serde_json::json!({})),
                nums(&[1, 2, 3]),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(out.get("count"), Some(&Value::I64(3)));
        assert_eq!(out.get("errors"), Some(&Value::I64(0)));
        let results = match out.get("results") {
            Some(Value::Vec(v)) => v,
            other => panic!("expected results vec, got {other:?}"),
        };
        let doubled: Vec<i64> = results
            .iter()
            .map(|r| match r {
                Value::Map(m) => match m.get("doubled") {
                    Some(Value::I64(n)) => *n,
                    _ => -1,
                },
                _ => -1,
            })
            .collect();
        assert_eq!(doubled, vec![2, 4, 6]); // ordered by index
    }

    #[tokio::test]
    async fn map_quorum_votes() {
        let mut lib = GraphLibrary::new();
        lib.register("big", child_with("is_big"));
        let h = MapHandler::with_handlers(Arc::new(lib), handlers());

        let reduce = serde_json::json!({ "reduce": "quorum", "quorum": { "field": "real", "threshold": 2 } });
        let out = h
            .execute(
                &map_node("big", reduce),
                nums(&[1, 2, 3]),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        // items 2 and 3 are "big" -> 2 votes, threshold 2 -> passed
        assert_eq!(out.get("votes"), Some(&Value::I64(2)));
        assert_eq!(out.get("passed"), Some(&Value::Bool(true)));
    }

    #[tokio::test]
    async fn map_skips_failed_items_by_default() {
        let mut lib = GraphLibrary::new();
        lib.register("ft", child_with("fail_on_two"));
        let h = MapHandler::with_handlers(Arc::new(lib), handlers());

        let out = h
            .execute(
                &map_node("ft", serde_json::json!({})),
                nums(&[1, 2, 3]),
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(out.get("errors"), Some(&Value::I64(1))); // item 2 failed
        assert_eq!(out.get("count"), Some(&Value::I64(2))); // 1 and 3 survived
    }

    #[tokio::test]
    async fn map_fail_mode_propagates() {
        let mut lib = GraphLibrary::new();
        lib.register("ft", child_with("fail_on_two"));
        let h = MapHandler::with_handlers(Arc::new(lib), handlers());

        let reduce = serde_json::json!({ "on_item_error": "fail" });
        let result = h
            .execute(
                &map_node("ft", reduce),
                nums(&[1, 2, 3]),
                CancellationToken::new(),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn map_requires_list_input() {
        let mut lib = GraphLibrary::new();
        lib.register("dbl", child_with("double"));
        let h = MapHandler::with_handlers(Arc::new(lib), handlers());

        // `nums` present but not a list
        let mut bad = Outputs::new();
        bad.insert("nums".into(), Value::I64(5));
        let result = h
            .execute(
                &map_node("dbl", serde_json::json!({})),
                bad,
                CancellationToken::new(),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("not a list"));

        // missing entirely
        let result = h
            .execute(
                &map_node("dbl", serde_json::json!({})),
                Outputs::new(),
                CancellationToken::new(),
            )
            .await;
        assert!(result.unwrap_err().to_string().contains("missing input"));
    }
}
