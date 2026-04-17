//! Workflow control handlers: `break` and `select`.
//!
//! These were originally implemented in ergon-core and upstreamed (SSOT
//! §6.4.2) so other embedders can reuse them.

use crate::blackboard::helpers;
use crate::error::NodeError;
use crate::execute::{CancellationToken, ExecutionContext, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::graph::types::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// BreakHandler — signal the loop controller to end the current loop
// ---------------------------------------------------------------------------

/// Handler for `break` nodes — signals the loop controller to exit the
/// current loop after this iteration finishes.
///
/// Writes the break flag via [`helpers::set_break_signal`]. A loop iterator
/// checks [`helpers::has_break_signal`] between iterations and stops early if
/// set. The embedder is responsible for pairing the iterator with a call to
/// [`helpers::clear_break_signal`] at the start of each loop run.
///
/// Produces a single output `out = true` so downstream nodes wired to the
/// break node observe a definite value.
///
/// ## Construction
///
/// Requires an [`ExecutionContext`] to write to the blackboard. Constructed
/// via `BreakHandler::new(ctx)`.
pub struct BreakHandler {
    exec_ctx: Arc<ExecutionContext>,
}

impl BreakHandler {
    pub fn new(ctx: Arc<ExecutionContext>) -> Self {
        Self { exec_ctx: ctx }
    }
}

impl NodeHandler for BreakHandler {
    fn execute(
        &self,
        _node: &Node,
        _inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let ctx = self.exec_ctx.clone();
        Box::pin(async move {
            {
                let mut bb = ctx.blackboard();
                helpers::set_break_signal(&mut bb);
            }
            let mut outputs = Outputs::new();
            outputs.insert("out".into(), Value::Bool(true));
            Ok(outputs)
        })
    }
}

// ---------------------------------------------------------------------------
// SelectHandler — pick values out of workflow state
// ---------------------------------------------------------------------------

/// Handler for `select` nodes — extract a value from workflow state by
/// dotted path, optionally projecting or joining array contents.
///
/// ## Configuration
///
/// - `config.from` (required) — dotted path into workflow state. The first
///   segment names the namespace: `inputs`, `results`, `constants`, or
///   `output_dir`. Subsequent segments navigate into maps or arrays.
/// - `config.keys` — when the resolved value is an array of objects, picks
///   this single key from each object and returns the list of picked values.
/// - `config.join` — when present together with `config.keys`, joins the
///   projected list with this separator into a single string.
///
/// ## Output
///
/// The resolved/projected value under `out`. Also written into the workflow
/// results map under this node's id via [`helpers::set_result`] with the
/// default [`ResultReducer::Replace`] so downstream template expansion can
/// reference `results.{node_id}`.
///
/// ## Construction
///
/// Requires an [`ExecutionContext`] for blackboard access. Constructed via
/// `SelectHandler::new(ctx)`.
pub struct SelectHandler {
    exec_ctx: Arc<ExecutionContext>,
}

impl SelectHandler {
    pub fn new(ctx: Arc<ExecutionContext>) -> Self {
        Self { exec_ctx: ctx }
    }
}

impl NodeHandler for SelectHandler {
    fn execute(
        &self,
        node: &Node,
        _inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let from_path = node
            .config
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let keys = node
            .config
            .get("keys")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let join = node
            .config
            .get("join")
            .and_then(|v| v.as_str())
            .map(str::to_owned);
        let node_id = node.id.0.clone();
        let ctx = self.exec_ctx.clone();

        Box::pin(async move {
            let view = {
                let bb = ctx.blackboard();
                helpers::build_context_maps(&bb)
            };

            let source = resolve_path(&from_path, &view);

            let result = match (source, keys, join) {
                (Some(serde_json::Value::Array(arr)), Some(keys_str), join_str) => {
                    let selected: Vec<String> = arr
                        .iter()
                        .filter_map(|item| {
                            item.get(&keys_str)
                                .and_then(|v| v.as_str())
                                .map(str::to_owned)
                        })
                        .collect();
                    if let Some(sep) = join_str {
                        serde_json::Value::String(selected.join(&sep))
                    } else {
                        serde_json::json!(selected)
                    }
                }
                (Some(val), _, _) => val,
                _ => serde_json::Value::String(from_path),
            };

            // Write into workflow results under this node's id.
            {
                let mut bb = ctx.blackboard();
                helpers::set_result(
                    &mut bb,
                    &node_id,
                    result.clone(),
                    &crate::graph::types::ResultReducer::Replace,
                );
            }

            let mut outputs = Outputs::new();
            outputs.insert("out".into(), Value::from(result));
            Ok(outputs)
        })
    }
}

/// Resolve a dotted path into workflow state.
///
/// The first segment selects a namespace; remaining segments walk JSON maps
/// and arrays. Returns `None` if any segment is missing.
fn resolve_path(path: &str, view: &helpers::WorkflowStateView) -> Option<serde_json::Value> {
    let mut parts = path.splitn(2, '.');
    let namespace = parts.next()?;
    let rest = parts.next().unwrap_or("");

    let root = match namespace {
        "inputs" => map_to_json(&view.inputs),
        "results" => map_to_json(&view.results),
        "constants" => map_to_json(&view.constants),
        "output_dir" => return view.output_dir.clone().map(serde_json::Value::String),
        _ => return None,
    };

    if rest.is_empty() {
        return Some(root);
    }

    navigate(&root, rest)
}

fn map_to_json(m: &std::collections::BTreeMap<String, serde_json::Value>) -> serde_json::Value {
    serde_json::Value::Object(m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

fn navigate(value: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    if path.is_empty() {
        return Some(value.clone());
    }
    let mut current = value.clone();
    for segment in path.split('.') {
        match &current {
            serde_json::Value::Object(obj) => {
                current = obj.get(segment)?.clone();
            }
            serde_json::Value::Array(arr) => {
                let idx: usize = segment.parse().ok()?;
                current = arr.get(idx)?.clone();
            }
            _ => return None,
        }
    }
    Some(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::node::Node;
    use std::collections::BTreeMap;

    fn make_ctx() -> Arc<ExecutionContext> {
        Arc::new(ExecutionContext::new())
    }

    #[tokio::test]
    async fn break_handler_raises_signal() {
        let ctx = make_ctx();
        let handler = BreakHandler::new(ctx.clone());
        let node = Node::new("brk", "Break").with_handler("break");

        let outputs = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(outputs.get("out"), Some(&Value::Bool(true)));
        let bb = ctx.blackboard();
        assert!(helpers::has_break_signal(&bb));
    }

    #[tokio::test]
    async fn select_array_with_keys_and_join() {
        let ctx = make_ctx();
        {
            let mut bb = ctx.blackboard();
            helpers::init(&mut bb, &BTreeMap::new(), &BTreeMap::new());
            let mut results = BTreeMap::new();
            results.insert(
                "items".into(),
                serde_json::json!([
                    {"name": "foo"},
                    {"name": "bar"},
                    {"name": "baz"},
                ]),
            );
            helpers::write_map(&mut bb, helpers::WORKFLOW_RESULTS, &results);
        }

        let handler = SelectHandler::new(ctx.clone());
        let mut node = Node::new("sel", "Select").with_handler("select");
        node.config = serde_json::json!({
            "from": "results.items",
            "keys": "name",
            "join": ", ",
        });

        let outputs = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        let json: serde_json::Value = outputs.get("out").unwrap().clone().into();
        assert_eq!(json, serde_json::json!("foo, bar, baz"));
    }

    #[tokio::test]
    async fn select_array_with_keys_no_join() {
        let ctx = make_ctx();
        {
            let mut bb = ctx.blackboard();
            helpers::init(&mut bb, &BTreeMap::new(), &BTreeMap::new());
            let mut results = BTreeMap::new();
            results.insert(
                "items".into(),
                serde_json::json!([
                    {"name": "foo"},
                    {"name": "bar"},
                ]),
            );
            helpers::write_map(&mut bb, helpers::WORKFLOW_RESULTS, &results);
        }

        let handler = SelectHandler::new(ctx);
        let mut node = Node::new("sel", "Select").with_handler("select");
        node.config = serde_json::json!({
            "from": "results.items",
            "keys": "name",
        });

        let outputs = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        let json: serde_json::Value = outputs.get("out").unwrap().clone().into();
        assert_eq!(json, serde_json::json!(["foo", "bar"]));
    }

    #[tokio::test]
    async fn select_scalar_by_path() {
        let ctx = make_ctx();
        {
            let mut bb = ctx.blackboard();
            let mut inputs = BTreeMap::new();
            inputs.insert("topic".into(), serde_json::json!("Rust"));
            helpers::init(&mut bb, &inputs, &BTreeMap::new());
        }

        let handler = SelectHandler::new(ctx);
        let mut node = Node::new("sel", "Select").with_handler("select");
        node.config = serde_json::json!({ "from": "inputs.topic" });

        let outputs = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        let json: serde_json::Value = outputs.get("out").unwrap().clone().into();
        assert_eq!(json, serde_json::json!("Rust"));
    }

    #[tokio::test]
    async fn select_stores_result_in_blackboard() {
        let ctx = make_ctx();
        {
            let mut bb = ctx.blackboard();
            let mut inputs = BTreeMap::new();
            inputs.insert("topic".into(), serde_json::json!("Rust"));
            helpers::init(&mut bb, &inputs, &BTreeMap::new());
        }

        let handler = SelectHandler::new(ctx.clone());
        let mut node = Node::new("picked", "Select").with_handler("select");
        node.config = serde_json::json!({ "from": "inputs.topic" });

        handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        let bb = ctx.blackboard();
        let results = helpers::read_map(&bb, helpers::WORKFLOW_RESULTS);
        assert_eq!(results.get("picked"), Some(&serde_json::json!("Rust")));
    }

    #[tokio::test]
    async fn select_unresolved_path_returns_literal() {
        let ctx = make_ctx();
        {
            let mut bb = ctx.blackboard();
            helpers::init(&mut bb, &BTreeMap::new(), &BTreeMap::new());
        }

        let handler = SelectHandler::new(ctx);
        let mut node = Node::new("sel", "Select").with_handler("select");
        node.config = serde_json::json!({ "from": "nope.missing" });

        let outputs = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        let json: serde_json::Value = outputs.get("out").unwrap().clone().into();
        assert_eq!(json, serde_json::json!("nope.missing"));
    }

    #[test]
    fn resolve_path_navigates_nested_map() {
        let mut inputs = BTreeMap::new();
        inputs.insert(
            "nested".into(),
            serde_json::json!({"inner": {"key": "value"}}),
        );
        let view = helpers::WorkflowStateView {
            inputs,
            results: BTreeMap::new(),
            constants: BTreeMap::new(),
            loop_vars: None,
            output_dir: None,
        };

        assert_eq!(
            resolve_path("inputs.nested.inner.key", &view),
            Some(serde_json::json!("value"))
        );
    }

    #[test]
    fn resolve_path_output_dir() {
        let view = helpers::WorkflowStateView {
            inputs: BTreeMap::new(),
            results: BTreeMap::new(),
            constants: BTreeMap::new(),
            loop_vars: None,
            output_dir: Some("/tmp/out".into()),
        };
        assert_eq!(
            resolve_path("output_dir", &view),
            Some(serde_json::json!("/tmp/out"))
        );
    }
}
