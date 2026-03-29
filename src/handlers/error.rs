use crate::error::NodeError;
use crate::execute::retry::RetryConfig;
use crate::execute::{CancellationToken, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::graph::types::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Catch — wrap a child handler, route errors to outputs instead of failing
// ---------------------------------------------------------------------------

/// Wraps another handler. If the inner handler fails, the error is captured
/// as output values instead of propagating as a node failure.
///
/// On success: forwards the inner handler's outputs.
/// On error: produces `{"error": "message", "error_type": "Failed|Timeout|..."}`.
///
/// This allows downstream nodes to handle errors as data.
pub struct CatchHandler {
    inner: Arc<dyn NodeHandler>,
}

impl CatchHandler {
    pub fn new(inner: Arc<dyn NodeHandler>) -> Self {
        Self { inner }
    }
}

impl NodeHandler for CatchHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let inner = self.inner.clone();
        let node = node.clone();

        Box::pin(async move {
            match inner.execute(&node, inputs, cancel).await {
                Ok(outputs) => {
                    let mut result = outputs;
                    result.insert("_caught_error".into(), Value::Bool(false));
                    Ok(result)
                }
                Err(e) => {
                    let mut outputs = Outputs::new();
                    outputs.insert("_caught_error".into(), Value::Bool(true));
                    outputs.insert("error".into(), Value::String(e.to_string()));
                    outputs.insert("error_type".into(), Value::String(error_type_name(&e)));
                    outputs.insert(
                        "recoverable".into(),
                        Value::Bool(is_recoverable(&e)),
                    );
                    Ok(outputs)
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Fallback — try primary handler, fall back to secondary on failure
// ---------------------------------------------------------------------------

/// Tries the primary handler first. If it fails, runs the fallback handler
/// with the same inputs. If both fail, the fallback's error propagates.
pub struct FallbackHandler {
    primary: Arc<dyn NodeHandler>,
    fallback: Arc<dyn NodeHandler>,
}

impl FallbackHandler {
    pub fn new(primary: Arc<dyn NodeHandler>, fallback: Arc<dyn NodeHandler>) -> Self {
        Self { primary, fallback }
    }
}

impl NodeHandler for FallbackHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let primary = self.primary.clone();
        let fallback = self.fallback.clone();
        let node = node.clone();
        let inputs_backup = inputs.clone();

        Box::pin(async move {
            match primary.execute(&node, inputs, cancel.clone()).await {
                Ok(outputs) => Ok(outputs),
                Err(primary_err) => {
                    match fallback.execute(&node, inputs_backup, cancel).await {
                        Ok(outputs) => Ok(outputs),
                        Err(fallback_err) => Err(NodeError::Failed {
                            source_message: Some(primary_err.to_string()),
                            message: format!("fallback also failed: {fallback_err}"),
                            recoverable: is_recoverable(&fallback_err),
                        }),
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// ErrorTransform — reshape error information in outputs
// ---------------------------------------------------------------------------

/// Transforms error-related output values based on `config.transform`.
///
/// Supported transforms:
/// - `"simplify"`: Replaces detailed error with just the error type.
/// - `"enrich"`: Adds `_error_node` (node ID) to error outputs.
/// - Custom key mapping via `config.field_map`: `{"error": "failure_reason"}`.
pub struct ErrorTransformHandler;

impl NodeHandler for ErrorTransformHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let transform = node
            .config
            .get("transform")
            .and_then(|v| v.as_str())
            .unwrap_or("enrich")
            .to_string();
        let node_id = node.id.0.clone();
        let field_map: std::collections::HashMap<String, String> = node
            .config
            .get("field_map")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();

        Box::pin(async move {
            let mut outputs = Outputs::new();

            match transform.as_str() {
                "simplify" => {
                    // Keep only error_type, drop detailed message
                    if let Some(et) = inputs.get("error_type") {
                        outputs.insert("error".into(), et.clone());
                    }
                    // Forward non-error fields
                    for (k, v) in &inputs {
                        if k != "error" && k != "error_type" && k != "recoverable" {
                            outputs.insert(k.clone(), v.clone());
                        }
                    }
                }
                "enrich" => {
                    outputs = inputs;
                    outputs.insert("_error_node".into(), Value::String(node_id));
                }
                _ => {
                    outputs = inputs;
                }
            }

            // Apply custom field mapping
            if !field_map.is_empty() {
                let mut remapped = Outputs::new();
                for (k, v) in outputs {
                    let new_key = field_map.get(&k).cloned().unwrap_or(k);
                    remapped.insert(new_key, v);
                }
                outputs = remapped;
            }

            Ok(outputs)
        })
    }
}

// ---------------------------------------------------------------------------
// Retry — composable retry wrapper for any handler
// ---------------------------------------------------------------------------

/// Composable retry wrapper. Wraps any `NodeHandler` with retry-on-failure logic.
///
/// This is the handler-level equivalent of the exec-level retry in the executor.
/// Useful when you want to configure retry programmatically rather than via annotations.
pub struct RetryHandler {
    inner: Arc<dyn NodeHandler>,
    config: RetryConfig,
}

impl RetryHandler {
    pub fn new(inner: Arc<dyn NodeHandler>, config: RetryConfig) -> Self {
        Self { inner, config }
    }
}

impl NodeHandler for RetryHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let inner = self.inner.clone();
        let config = self.config.clone();
        let node = node.clone();

        Box::pin(async move {
            crate::execute::retry::execute_with_retry(&inner, &node, inputs, cancel, &config).await
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn error_type_name(e: &NodeError) -> String {
    match e {
        NodeError::Failed { .. } => "Failed".into(),
        NodeError::Timeout { .. } => "Timeout".into(),
        NodeError::Cancelled { .. } => "Cancelled".into(),
        NodeError::TypeMismatch { .. } => "TypeMismatch".into(),
        NodeError::AdapterError { .. } => "AdapterError".into(),
    }
}

fn is_recoverable(e: &NodeError) -> bool {
    match e {
        NodeError::Failed { recoverable, .. } => *recoverable,
        NodeError::Timeout { .. } => true,
        NodeError::Cancelled { .. } => false,
        NodeError::TypeMismatch { .. } => false,
        NodeError::AdapterError { .. } => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::sync_handler;

    fn run_handler(
        handler: &dyn NodeHandler,
        node: &Node,
        inputs: Outputs,
    ) -> Result<Outputs, NodeError> {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(handler.execute(node, inputs, CancellationToken::new()))
    }

    fn ok_handler() -> Arc<dyn NodeHandler> {
        sync_handler(|_, mut inputs| {
            inputs.insert("result".into(), Value::String("ok".into()));
            Ok(inputs)
        })
    }

    fn fail_handler() -> Arc<dyn NodeHandler> {
        sync_handler(|_, _| {
            Err(NodeError::Failed {
                source_message: None,
                message: "intentional failure".into(),
                recoverable: true,
            })
        })
    }

    fn timeout_handler() -> Arc<dyn NodeHandler> {
        sync_handler(|_, _| {
            Err(NodeError::Timeout {
                elapsed_ms: 1000,
                limit_ms: 500,
            })
        })
    }

    // -- Catch tests --

    #[test]
    fn catch_success_passes_through() {
        let handler = CatchHandler::new(ok_handler());
        let result = run_handler(&handler, &Node::new("C", "Catch"), Outputs::new()).unwrap();

        assert_eq!(result.get("_caught_error"), Some(&Value::Bool(false)));
        assert_eq!(result.get("result"), Some(&Value::String("ok".into())));
    }

    #[test]
    fn catch_failure_captures_error() {
        let handler = CatchHandler::new(fail_handler());
        let result = run_handler(&handler, &Node::new("C", "Catch"), Outputs::new()).unwrap();

        assert_eq!(result.get("_caught_error"), Some(&Value::Bool(true)));
        assert_eq!(result.get("error_type"), Some(&Value::String("Failed".into())));
        assert_eq!(result.get("recoverable"), Some(&Value::Bool(true)));
        assert!(result.get("error").is_some());
    }

    #[test]
    fn catch_timeout_captures_error() {
        let handler = CatchHandler::new(timeout_handler());
        let result = run_handler(&handler, &Node::new("C", "Catch"), Outputs::new()).unwrap();

        assert_eq!(result.get("_caught_error"), Some(&Value::Bool(true)));
        assert_eq!(result.get("error_type"), Some(&Value::String("Timeout".into())));
        assert_eq!(result.get("recoverable"), Some(&Value::Bool(true)));
    }

    // -- Fallback tests --

    #[test]
    fn fallback_primary_succeeds() {
        let handler = FallbackHandler::new(ok_handler(), fail_handler());
        let result = run_handler(&handler, &Node::new("F", "Fallback"), Outputs::new()).unwrap();

        assert_eq!(result.get("result"), Some(&Value::String("ok".into())));
    }

    #[test]
    fn fallback_primary_fails_uses_secondary() {
        let handler = FallbackHandler::new(fail_handler(), ok_handler());
        let result = run_handler(&handler, &Node::new("F", "Fallback"), Outputs::new()).unwrap();

        assert_eq!(result.get("result"), Some(&Value::String("ok".into())));
    }

    #[test]
    fn fallback_both_fail_includes_both_errors() {
        let handler = FallbackHandler::new(fail_handler(), timeout_handler());
        let result = run_handler(&handler, &Node::new("F", "Fallback"), Outputs::new());

        match result {
            Err(NodeError::Failed {
                source_message,
                message,
                ..
            }) => {
                // Primary error preserved in source_message
                assert!(source_message.unwrap().contains("intentional failure"));
                // Fallback error in message
                assert!(message.contains("fallback also failed"));
            }
            other => panic!("expected Failed with both errors, got {other:?}"),
        }
    }

    // -- ErrorTransform tests --

    #[test]
    fn error_transform_enrich() {
        let mut node = Node::new("ET", "ErrorTransform");
        node.config = serde_json::json!({ "transform": "enrich" });

        let mut inputs = Outputs::new();
        inputs.insert("error".into(), Value::String("something broke".into()));
        inputs.insert("error_type".into(), Value::String("Failed".into()));

        let result = run_handler(&ErrorTransformHandler, &node, inputs).unwrap();
        assert_eq!(result.get("_error_node"), Some(&Value::String("ET".into())));
        assert!(result.contains_key("error"));
    }

    #[test]
    fn error_transform_simplify() {
        let mut node = Node::new("ET", "ErrorTransform");
        node.config = serde_json::json!({ "transform": "simplify" });

        let mut inputs = Outputs::new();
        inputs.insert("error".into(), Value::String("detailed message".into()));
        inputs.insert("error_type".into(), Value::String("Timeout".into()));
        inputs.insert("recoverable".into(), Value::Bool(true));
        inputs.insert("data".into(), Value::I64(42));

        let result = run_handler(&ErrorTransformHandler, &node, inputs).unwrap();
        // Error replaced with just the type
        assert_eq!(result.get("error"), Some(&Value::String("Timeout".into())));
        // Non-error fields preserved
        assert_eq!(result.get("data"), Some(&Value::I64(42)));
        // Detailed fields removed
        assert!(!result.contains_key("error_type"));
        assert!(!result.contains_key("recoverable"));
    }

    #[test]
    fn error_transform_field_map() {
        let mut node = Node::new("ET", "ErrorTransform");
        node.config = serde_json::json!({
            "transform": "enrich",
            "field_map": {"error": "failure_reason"}
        });

        let mut inputs = Outputs::new();
        inputs.insert("error".into(), Value::String("broke".into()));

        let result = run_handler(&ErrorTransformHandler, &node, inputs).unwrap();
        assert!(result.contains_key("failure_reason"));
        assert!(!result.contains_key("error"));
    }
}
