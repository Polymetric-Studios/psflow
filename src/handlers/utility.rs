use crate::error::NodeError;
use crate::execute::{CancellationToken, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::graph::types::Value;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Passthrough — forward all inputs as outputs unchanged
// ---------------------------------------------------------------------------

pub struct PassthroughHandler;

impl NodeHandler for PassthroughHandler {
    fn execute(
        &self,
        _node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        Box::pin(async move { Ok(inputs) })
    }
}

// ---------------------------------------------------------------------------
// Transform / Map — apply a key mapping from config
// ---------------------------------------------------------------------------

/// Renames output keys based on `config.mapping`: `{"old_key": "new_key", ...}`.
/// Keys not in the mapping pass through unchanged.
pub struct TransformHandler;

impl NodeHandler for TransformHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let mapping: HashMap<String, String> = node
            .config
            .get("mapping")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        let mut outputs = Outputs::new();
        for (key, value) in inputs {
            let new_key = mapping.get(&key).cloned().unwrap_or(key);
            outputs.insert(new_key, value);
        }
        Box::pin(async move { Ok(outputs) })
    }
}

// ---------------------------------------------------------------------------
// Delay — wait for a configured duration before passing through
// ---------------------------------------------------------------------------

/// Waits `config.delay_ms` milliseconds, then forwards inputs as outputs.
/// Respects cancellation during the wait.
pub struct DelayHandler;

impl NodeHandler for DelayHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let delay_ms = node
            .config
            .get("delay_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        Box::pin(async move {
            if delay_ms > 0 {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {}
                    _ = cancel.cancelled() => {
                        return Err(NodeError::Cancelled {
                            reason: "cancelled during delay".into(),
                        });
                    }
                }
            }
            Ok(inputs)
        })
    }
}

// ---------------------------------------------------------------------------
// Log — emit inputs as a log event, then pass through
// ---------------------------------------------------------------------------

/// Passes inputs through unchanged. The node label is used as the log prefix.
/// In a real system this would integrate with `tracing`; for now it emits to stderr.
pub struct LogHandler;

impl NodeHandler for LogHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let label = node.label.clone();
        let node_id = node.id.0.clone();
        let logged = inputs.clone();
        Box::pin(async move {
            eprintln!("[log:{node_id}] {label}: {logged:?}");
            Ok(inputs)
        })
    }
}

// ---------------------------------------------------------------------------
// Merge — combine multiple inputs into a single output map
// ---------------------------------------------------------------------------

/// Collects all inputs into a single `"merged"` output containing a Map value.
/// Useful for fan-in after parallel branches.
pub struct MergeHandler;

impl NodeHandler for MergeHandler {
    fn execute(
        &self,
        _node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let merged: std::collections::BTreeMap<String, Value> = inputs.into_iter().collect();

        let mut outputs = Outputs::new();
        outputs.insert("merged".into(), Value::Map(merged));
        Box::pin(async move { Ok(outputs) })
    }
}

// ---------------------------------------------------------------------------
// Split — take a Map or Vec input and fan out into individual outputs
// ---------------------------------------------------------------------------

/// Takes a `config.input_key` (default `"data"`) input and splits it:
/// - If `Value::Map`: each entry becomes a separate output key.
/// - If `Value::Vec`: outputs `"item_0"`, `"item_1"`, etc.
/// - Otherwise: passes through as `"data"`.
pub struct SplitHandler;

impl NodeHandler for SplitHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let input_key = node
            .config
            .get("input_key")
            .and_then(|v| v.as_str())
            .unwrap_or("data");

        let mut outputs = Outputs::new();

        if let Some(value) = inputs.get(input_key) {
            match value {
                Value::Map(map) => {
                    for (k, v) in map {
                        outputs.insert(k.clone(), v.clone());
                    }
                }
                Value::Vec(items) => {
                    for (i, v) in items.iter().enumerate() {
                        outputs.insert(format!("item_{i}"), v.clone());
                    }
                }
                other => {
                    outputs.insert("data".into(), other.clone());
                }
            }
        }

        Box::pin(async move { Ok(outputs) })
    }
}

// ---------------------------------------------------------------------------
// Gate — conditional pass-through based on a guard expression
// ---------------------------------------------------------------------------

/// Passes inputs through only if `config.guard` evaluates to truthy.
/// If the guard is falsy, produces empty outputs (effectively blocking data flow).
pub struct GateHandler;

impl NodeHandler for GateHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let guard_expr = node
            .config
            .get("guard")
            .and_then(|v| v.as_str())
            .unwrap_or("true")
            .to_string();

        Box::pin(async move {
            let bb = crate::execute::blackboard::Blackboard::new();
            let result = crate::execute::control::evaluate_guard(&guard_expr, &inputs, &bb)
                .unwrap_or(crate::execute::control::GuardResult::Bool(false));

            if result == crate::execute::control::GuardResult::Bool(true) {
                Ok(inputs)
            } else {
                Ok(Outputs::new())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::node::Node;

    fn run_handler(
        handler: &dyn NodeHandler,
        node: &Node,
        inputs: Outputs,
    ) -> Result<Outputs, NodeError> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(handler.execute(node, inputs, CancellationToken::new()))
    }

    #[test]
    fn passthrough_forwards_all() {
        let mut inputs = Outputs::new();
        inputs.insert("a".into(), Value::I64(1));
        inputs.insert("b".into(), Value::String("hello".into()));

        let result = run_handler(&PassthroughHandler, &Node::new("N", "N"), inputs.clone()).unwrap();
        assert_eq!(result, inputs);
    }

    #[test]
    fn transform_renames_keys() {
        let mut node = Node::new("T", "Transform");
        node.config = serde_json::json!({
            "mapping": {"old": "new"}
        });

        let mut inputs = Outputs::new();
        inputs.insert("old".into(), Value::I64(42));
        inputs.insert("keep".into(), Value::Bool(true));

        let result = run_handler(&TransformHandler, &node, inputs).unwrap();
        assert_eq!(result.get("new"), Some(&Value::I64(42)));
        assert_eq!(result.get("keep"), Some(&Value::Bool(true)));
        assert!(!result.contains_key("old"));
    }

    #[test]
    fn transform_no_mapping_passes_through() {
        let node = Node::new("T", "Transform");
        let mut inputs = Outputs::new();
        inputs.insert("x".into(), Value::I64(1));

        let result = run_handler(&TransformHandler, &node, inputs.clone()).unwrap();
        assert_eq!(result, inputs);
    }

    #[tokio::test]
    async fn delay_waits_then_passes_through() {
        let mut node = Node::new("D", "Delay");
        node.config = serde_json::json!({ "delay_ms": 10 });

        let mut inputs = Outputs::new();
        inputs.insert("x".into(), Value::I64(1));

        let start = std::time::Instant::now();
        let result = DelayHandler
            .execute(&node, inputs.clone(), CancellationToken::new())
            .await
            .unwrap();
        assert!(start.elapsed().as_millis() >= 9);
        assert_eq!(result, inputs);
    }

    #[tokio::test]
    async fn delay_respects_cancellation() {
        let mut node = Node::new("D", "Delay");
        node.config = serde_json::json!({ "delay_ms": 5000 });

        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();

        let handle = tokio::spawn(async move {
            DelayHandler
                .execute(&node, Outputs::new(), cancel2)
                .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();

        let result = handle.await.unwrap();
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    #[test]
    fn log_passes_through() {
        let mut inputs = Outputs::new();
        inputs.insert("msg".into(), Value::String("test".into()));

        let result = run_handler(&LogHandler, &Node::new("L", "Log"), inputs.clone()).unwrap();
        assert_eq!(result, inputs);
    }

    #[test]
    fn merge_combines_inputs() {
        let mut inputs = Outputs::new();
        inputs.insert("a".into(), Value::I64(1));
        inputs.insert("b".into(), Value::I64(2));

        let result = run_handler(&MergeHandler, &Node::new("M", "Merge"), inputs).unwrap();
        match result.get("merged") {
            Some(Value::Map(map)) => {
                assert_eq!(map.len(), 2);
                assert_eq!(map.get("a"), Some(&Value::I64(1)));
                assert_eq!(map.get("b"), Some(&Value::I64(2)));
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }

    #[test]
    fn split_map_into_keys() {
        let mut map = std::collections::BTreeMap::new();
        map.insert("x".into(), Value::I64(1));
        map.insert("y".into(), Value::I64(2));

        let mut inputs = Outputs::new();
        inputs.insert("data".into(), Value::Map(map));

        let result = run_handler(&SplitHandler, &Node::new("S", "Split"), inputs).unwrap();
        assert_eq!(result.get("x"), Some(&Value::I64(1)));
        assert_eq!(result.get("y"), Some(&Value::I64(2)));
    }

    #[test]
    fn split_vec_into_indexed_keys() {
        let items = vec![Value::String("a".into()), Value::String("b".into())];
        let mut inputs = Outputs::new();
        inputs.insert("data".into(), Value::Vec(items));

        let result = run_handler(&SplitHandler, &Node::new("S", "Split"), inputs).unwrap();
        assert_eq!(result.get("item_0"), Some(&Value::String("a".into())));
        assert_eq!(result.get("item_1"), Some(&Value::String("b".into())));
    }

    #[test]
    fn split_scalar_passes_as_data() {
        let mut inputs = Outputs::new();
        inputs.insert("data".into(), Value::I64(42));

        let result = run_handler(&SplitHandler, &Node::new("S", "Split"), inputs).unwrap();
        assert_eq!(result.get("data"), Some(&Value::I64(42)));
    }

    #[test]
    fn split_custom_input_key() {
        let mut node = Node::new("S", "Split");
        node.config = serde_json::json!({ "input_key": "items" });

        let items = vec![Value::I64(10)];
        let mut inputs = Outputs::new();
        inputs.insert("items".into(), Value::Vec(items));

        let result = run_handler(&SplitHandler, &node, inputs).unwrap();
        assert_eq!(result.get("item_0"), Some(&Value::I64(10)));
    }

    #[test]
    fn gate_open_passes_through() {
        let mut node = Node::new("G", "Gate");
        node.config = serde_json::json!({ "guard": "inputs.flag == true" });

        let mut inputs = Outputs::new();
        inputs.insert("flag".into(), Value::Bool(true));
        inputs.insert("data".into(), Value::I64(42));

        let result = run_handler(&GateHandler, &node, inputs).unwrap();
        assert_eq!(result.get("data"), Some(&Value::I64(42)));
    }

    #[test]
    fn gate_closed_produces_empty() {
        let mut node = Node::new("G", "Gate");
        node.config = serde_json::json!({ "guard": "inputs.flag == true" });

        let mut inputs = Outputs::new();
        inputs.insert("flag".into(), Value::Bool(false));
        inputs.insert("data".into(), Value::I64(42));

        let result = run_handler(&GateHandler, &node, inputs).unwrap();
        assert!(result.is_empty());
    }
}
