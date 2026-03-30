use crate::error::NodeError;
use crate::execute::blackboard::{Blackboard, BlackboardScope};
use crate::execute::{CancellationToken, ExecutionContext, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::graph::types::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Accumulator handler: appends input data to a running collection on the blackboard.
///
/// Each invocation reads the current collection from the blackboard, appends the
/// new value, and writes it back. The accumulated collection is also emitted as output.
///
/// ## Configuration
///
/// - `config.key` (required): Blackboard key to store the accumulated collection.
/// - `config.input_key`: Input key to read the value to accumulate (default: `"value"`).
/// - `config.scope`: Blackboard scope: `"global"` (default), `"subgraph:<id>"`, or `"node:<id>"`.
///
/// ## Outputs
///
/// - `accumulated`: The full accumulated Vec after appending.
/// - `count`: Number of items in the collection (i64).
pub struct AccumulatorHandler {
    /// Shared execution context for blackboard access.
    exec_ctx: Arc<ExecutionContext>,
}

impl AccumulatorHandler {
    pub fn new(ctx: Arc<ExecutionContext>) -> Self {
        Self { exec_ctx: ctx }
    }
}

impl NodeHandler for AccumulatorHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let ctx = self.exec_ctx.clone();
        let config = node.config.clone();
        let node_id = node.id.0.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before accumulation".into(),
                });
            }

            let bb_key = config
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': missing config.key"),
                    recoverable: false,
                })?
                .to_string();

            let input_key = config
                .get("input_key")
                .and_then(|v| v.as_str())
                .unwrap_or("value");

            let scope = parse_scope(&config, &node_id)?;

            // Get the value to accumulate
            let new_value = inputs.get(input_key).cloned().unwrap_or(Value::Null);

            // Read current collection, append, write back
            let mut bb = ctx.blackboard();
            let mut collection = match bb.get(&bb_key, &scope) {
                Some(Value::Vec(existing)) => existing.clone(),
                Some(_) => {
                    // Key exists but isn't a Vec — wrap existing value and append
                    vec![bb.get(&bb_key, &scope).cloned().unwrap_or(Value::Null)]
                }
                None => Vec::new(),
            };

            collection.push(new_value);
            let count = collection.len() as i64;
            let accumulated = Value::Vec(collection);

            bb.set(bb_key, accumulated.clone(), scope);
            drop(bb);

            let mut outputs = Outputs::new();
            outputs.insert("accumulated".into(), accumulated);
            outputs.insert("count".into(), Value::I64(count));
            Ok(outputs)
        })
    }
}

fn parse_scope(config: &serde_json::Value, node_id: &str) -> Result<BlackboardScope, NodeError> {
    let scope_str = config
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("global");

    if scope_str == "global" {
        return Ok(BlackboardScope::Global);
    }
    if let Some(id) = scope_str.strip_prefix("subgraph:") {
        return Ok(BlackboardScope::Subgraph(id.to_string()));
    }
    if let Some(id) = scope_str.strip_prefix("node:") {
        return Ok(BlackboardScope::Node(id.to_string()));
    }

    Err(NodeError::Failed {
        source_message: None,
        message: format!(
            "node '{node_id}': invalid config.scope '{scope_str}'. \
             Use 'global', 'subgraph:<id>', or 'node:<id>'"
        ),
        recoverable: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ctx() -> Arc<ExecutionContext> {
        Arc::new(ExecutionContext::new())
    }

    #[tokio::test]
    async fn accumulate_values() {
        let ctx = make_ctx();
        let handler = AccumulatorHandler::new(ctx.clone());

        let mut node = Node::new("ACC", "Accumulate");
        node.config = serde_json::json!({ "key": "results" });

        // First call
        let mut inputs = Outputs::new();
        inputs.insert("value".into(), Value::String("first".into()));
        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result["count"], Value::I64(1));
        assert_eq!(
            result["accumulated"],
            Value::Vec(vec![Value::String("first".into())])
        );

        // Second call
        let mut inputs = Outputs::new();
        inputs.insert("value".into(), Value::String("second".into()));
        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result["count"], Value::I64(2));
        assert_eq!(
            result["accumulated"],
            Value::Vec(vec![
                Value::String("first".into()),
                Value::String("second".into()),
            ])
        );

        // Verify blackboard state
        let bb = ctx.blackboard();
        let stored = bb.get("results", &BlackboardScope::Global).unwrap();
        match stored {
            Value::Vec(v) => assert_eq!(v.len(), 2),
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn accumulate_custom_input_key() {
        let ctx = make_ctx();
        let handler = AccumulatorHandler::new(ctx.clone());

        let mut node = Node::new("ACC", "Accumulate");
        node.config = serde_json::json!({ "key": "items", "input_key": "data" });

        let mut inputs = Outputs::new();
        inputs.insert("data".into(), Value::I64(42));

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            result["accumulated"],
            Value::Vec(vec![Value::I64(42)])
        );
    }

    #[tokio::test]
    async fn accumulate_subgraph_scope() {
        let ctx = make_ctx();
        let handler = AccumulatorHandler::new(ctx.clone());

        let mut node = Node::new("ACC", "Accumulate");
        node.config = serde_json::json!({ "key": "local_data", "scope": "subgraph:sg1" });

        let mut inputs = Outputs::new();
        inputs.insert("value".into(), Value::Bool(true));

        handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        // Check scoped storage
        let bb = ctx.blackboard();
        let scope = BlackboardScope::Subgraph("sg1".into());
        assert!(bb.get("local_data", &scope).is_some());
        // Not visible in global
        assert!(bb.scope("sg1").unwrap().contains_key("local_data"));
    }

    #[tokio::test]
    async fn accumulate_missing_key_errors() {
        let ctx = make_ctx();
        let handler = AccumulatorHandler::new(ctx);

        let node = Node::new("ACC", "Accumulate");
        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing config.key"));
    }

    #[tokio::test]
    async fn accumulate_invalid_scope_errors() {
        let ctx = make_ctx();
        let handler = AccumulatorHandler::new(ctx);

        let mut node = Node::new("ACC", "Accumulate");
        node.config = serde_json::json!({ "key": "data", "scope": "invalid" });

        let mut inputs = Outputs::new();
        inputs.insert("value".into(), Value::I64(1));

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid config.scope"));
    }

    #[tokio::test]
    async fn accumulate_missing_input_appends_null() {
        let ctx = make_ctx();
        let handler = AccumulatorHandler::new(ctx);

        let mut node = Node::new("ACC", "Accumulate");
        node.config = serde_json::json!({ "key": "data" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            result["accumulated"],
            Value::Vec(vec![Value::Null])
        );
    }

    #[tokio::test]
    async fn accumulate_cancellation() {
        let ctx = make_ctx();
        let handler = AccumulatorHandler::new(ctx);

        let mut node = Node::new("ACC", "Accumulate");
        node.config = serde_json::json!({ "key": "data" });

        let token = CancellationToken::new();
        token.cancel();

        let result = handler.execute(&node, Outputs::new(), token).await;
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    #[test]
    fn parse_scope_variants() {
        assert_eq!(
            parse_scope(&serde_json::json!({}), "n").unwrap(),
            BlackboardScope::Global,
        );
        assert_eq!(
            parse_scope(&serde_json::json!({"scope": "global"}), "n").unwrap(),
            BlackboardScope::Global,
        );
        assert_eq!(
            parse_scope(&serde_json::json!({"scope": "subgraph:sg1"}), "n").unwrap(),
            BlackboardScope::Subgraph("sg1".into()),
        );
        assert_eq!(
            parse_scope(&serde_json::json!({"scope": "node:N1"}), "n").unwrap(),
            BlackboardScope::Node("N1".into()),
        );
        assert!(parse_scope(&serde_json::json!({"scope": "bad"}), "n").is_err());
    }
}
