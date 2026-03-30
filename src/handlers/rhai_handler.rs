//! Rhai script handler — executes inline or external `.rhai` scripts as node handlers.

use crate::error::NodeError;
use crate::execute::{CancellationToken, ExecutionContext, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::scripting::bridge::{dynamic_to_value, outputs_to_rhai_map, value_to_dynamic};
use crate::scripting::engine::{ScriptEngine, ScriptError};
use rhai::Dynamic;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

/// A `NodeHandler` that executes Rhai scripts.
///
/// Script source is determined from node config:
/// - `config.script` — inline script string
/// - `config.script_file` — path to external `.rhai` file
///
/// The script receives three variables in scope:
/// - `inputs` — a Rhai Map built from the node's input values
/// - `config` — a Rhai Map built from the node's config object
/// - `ctx` — a Rhai Map snapshot of the blackboard (global scope), if available
///
/// Blackboard access helper functions:
/// - `ctx_get(ctx, "key")` → look up a key in the ctx map (returns `()` if missing)
/// - `ctx_has(ctx, "key")` → check if a key exists in the ctx map
///
/// Annotation example:
/// ```text
/// %% @A handler: rhai
/// %% @A config.script: "let x = inputs.value * 2; #{ result: x }"
/// ```
pub struct RhaiHandler {
    engine: Arc<ScriptEngine>,
    exec_ctx: Arc<RwLock<Option<Arc<ExecutionContext>>>>,
}

impl RhaiHandler {
    pub fn new(engine: Arc<ScriptEngine>) -> Self {
        Self {
            engine,
            exec_ctx: Arc::new(RwLock::new(None)),
        }
    }

    /// Set the execution context for blackboard access.
    /// Can be called multiple times (e.g., for successive graph executions).
    pub fn set_context(&self, ctx: Arc<ExecutionContext>) {
        *self.exec_ctx.write().unwrap() = Some(ctx);
    }
}

impl NodeHandler for RhaiHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let engine = self.engine.clone();
        let config = node.config.clone();
        let node_id = node.id.clone();
        let exec_ctx = self.exec_ctx.clone();

        Box::pin(async move {
            // Determine script source
            let script = if let Some(inline) = config.get("script").and_then(|v| v.as_str()) {
                inline.to_string()
            } else if let Some(path) = config.get("script_file").and_then(|v| v.as_str()) {
                // SECURITY: Validate path stays within the current working directory
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string());
                let validated = crate::handlers::common::validate_path_containment(path, &cwd)
                    .map_err(|e| NodeError::Failed {
                        source_message: None,
                        message: format!("rhai handler [{node_id}]: {e}"),
                        recoverable: false,
                    })?;
                tokio::fs::read_to_string(&validated).await.map_err(|e| {
                    NodeError::Failed {
                        source_message: None,
                        message: format!(
                            "rhai handler [{node_id}]: failed to read script file '{path}': {e}"
                        ),
                        recoverable: false,
                    }
                })?
            } else {
                return Err(NodeError::Failed {
                    source_message: None,
                    message: format!(
                        "rhai handler [{node_id}]: missing 'script' or 'script_file' in config"
                    ),
                    recoverable: false,
                });
            };

            // Compile script
            let ast = engine.compile(&script).map_err(|e| NodeError::Failed {
                source_message: None,
                message: format!("rhai handler [{node_id}]: {e}"),
                recoverable: false,
            })?;

            // Build scope
            let mut scope = rhai::Scope::new();

            let inputs_map = outputs_to_rhai_map(&inputs);
            scope.push_dynamic("inputs", Dynamic::from_map(inputs_map));

            // Convert config JSON to Rhai Map (exclude script/script_file keys)
            let config_map = config_to_rhai_map(&config);
            scope.push_dynamic("config", Dynamic::from_map(config_map));

            // Inject blackboard snapshot as `ctx` Map if execution context is available
            if let Some(ctx) = exec_ctx.read().unwrap().as_ref() {
                let bb = ctx.blackboard();
                let mut ctx_map = rhai::Map::new();
                for (key, value) in bb.global() {
                    ctx_map.insert(key.clone().into(), value_to_dynamic(value));
                }
                drop(bb);
                scope.push_dynamic("ctx", Dynamic::from_map(ctx_map));
            } else {
                // No context available — inject empty ctx Map
                scope.push_dynamic("ctx", Dynamic::from_map(rhai::Map::new()));
            }

            // Execute
            let result = engine.eval_ast(&mut scope, &ast, &cancel).map_err(|e| {
                match e {
                    ScriptError::Cancelled => NodeError::Cancelled {
                        reason: format!("rhai handler [{node_id}]: script cancelled"),
                    },
                    other => NodeError::Failed {
                        source_message: None,
                        message: format!("rhai handler [{node_id}]: {other}"),
                        recoverable: false,
                    },
                }
            })?;

            // Convert result to Outputs
            if result.is_map() {
                let map: rhai::Map = result.cast();
                let outputs: Outputs = map
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), dynamic_to_value(v)))
                    .collect();
                Ok(outputs)
            } else {
                // Single value — wrap in a "result" key
                let mut outputs = Outputs::new();
                outputs.insert("result".into(), dynamic_to_value(result));
                Ok(outputs)
            }
        })
    }
}

/// Convert a `serde_json::Value` config object to a Rhai Map,
/// excluding script-source keys (`script`, `script_file`).
fn config_to_rhai_map(config: &serde_json::Value) -> rhai::Map {
    let mut map = rhai::Map::new();
    if let Some(obj) = config.as_object() {
        for (k, v) in obj {
            if k == "script" || k == "script_file" {
                continue;
            }
            map.insert(k.clone().into(), json_to_dynamic(v));
        }
    }
    map
}

fn json_to_dynamic(v: &serde_json::Value) -> Dynamic {
    use crate::graph::types::Value;
    value_to_dynamic(&Value::from(v.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::node::Node;
    use crate::graph::types::Value;
    use crate::scripting::engine::default_script_engine;

    fn make_node(config: serde_json::Value) -> Node {
        Node {
            id: "test".into(),
            label: "Test".into(),
            handler: Some("rhai".into()),
            config,
            exec: serde_json::Value::Object(Default::default()),
            inputs: vec![],
            outputs: vec![],
        }
    }

    #[tokio::test]
    async fn inline_script_returns_map() {
        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);
        let cancel = CancellationToken::new();

        let node = make_node(serde_json::json!({
            "script": "let x = inputs.value * 2; #{ result: x }"
        }));

        let mut inputs = Outputs::new();
        inputs.insert("value".into(), Value::I64(21));

        let outputs = handler.execute(&node, inputs, cancel).await.unwrap();
        assert_eq!(outputs.get("result"), Some(&Value::I64(42)));
    }

    #[tokio::test]
    async fn script_accesses_config() {
        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);
        let cancel = CancellationToken::new();

        let node = make_node(serde_json::json!({
            "script": "#{ greeting: `Hello ${config.name}!` }",
            "name": "World"
        }));

        let outputs = handler
            .execute(&node, Outputs::new(), cancel)
            .await
            .unwrap();
        assert_eq!(
            outputs.get("greeting"),
            Some(&Value::String("Hello World!".into()))
        );
    }

    #[tokio::test]
    async fn single_value_wrapped_in_result() {
        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);
        let cancel = CancellationToken::new();

        let node = make_node(serde_json::json!({
            "script": "inputs.x + inputs.y"
        }));

        let mut inputs = Outputs::new();
        inputs.insert("x".into(), Value::I64(3));
        inputs.insert("y".into(), Value::I64(4));

        let outputs = handler.execute(&node, inputs, cancel).await.unwrap();
        assert_eq!(outputs.get("result"), Some(&Value::I64(7)));
    }

    #[tokio::test]
    async fn missing_script_config_errors() {
        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);
        let cancel = CancellationToken::new();

        let node = make_node(serde_json::json!({}));

        let result = handler.execute(&node, Outputs::new(), cancel).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing"));
    }

    #[tokio::test]
    async fn parse_error_reported() {
        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);
        let cancel = CancellationToken::new();

        let node = make_node(serde_json::json!({
            "script": "let x = ;; garbage"
        }));

        let result = handler.execute(&node, Outputs::new(), cancel).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse"));
    }

    #[tokio::test]
    async fn cancellation_returns_cancelled_error() {
        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let node = make_node(serde_json::json!({
            "script": "42"
        }));

        let result = handler.execute(&node, Outputs::new(), cancel).await;
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    #[tokio::test]
    async fn script_file_path_traversal_blocked() {
        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);
        let cancel = CancellationToken::new();

        let node = make_node(serde_json::json!({
            "script_file": "/etc/passwd"
        }));

        let result = handler.execute(&node, Outputs::new(), cancel).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("escapes base directory"));
    }

    #[tokio::test]
    async fn script_file_not_found_errors() {
        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);
        let cancel = CancellationToken::new();

        // Use a relative path within cwd that doesn't exist
        let node = make_node(serde_json::json!({
            "script_file": "nonexistent_script.rhai"
        }));

        let result = handler.execute(&node, Outputs::new(), cancel).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("read script file"));
    }

    #[tokio::test]
    async fn script_with_external_file() {
        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);
        let cancel = CancellationToken::new();

        // Write a script file within the current working directory
        let cwd = std::env::current_dir().unwrap();
        let script_path = cwd.join("_test_rhai_handler.rhai");
        std::fs::write(
            &script_path,
            "let doubled = inputs.n * 2; #{ doubled: doubled }",
        )
        .unwrap();

        let node = make_node(serde_json::json!({
            "script_file": script_path.to_str().unwrap()
        }));

        let mut inputs = Outputs::new();
        inputs.insert("n".into(), Value::I64(5));

        let outputs = handler.execute(&node, inputs, cancel).await.unwrap();
        assert_eq!(outputs.get("doubled"), Some(&Value::I64(10)));

        // Cleanup
        let _ = std::fs::remove_file(&script_path);
    }

    #[tokio::test]
    async fn script_reads_blackboard_via_ctx() {
        use crate::execute::blackboard::BlackboardScope;

        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);

        // Set up execution context with blackboard data
        let exec_ctx = Arc::new(ExecutionContext::new());
        {
            let mut bb = exec_ctx.blackboard();
            bb.set(
                "threshold".into(),
                Value::I64(100),
                BlackboardScope::Global,
            );
            bb.set(
                "mode".into(),
                Value::String("fast".into()),
                BlackboardScope::Global,
            );
        }
        handler.set_context(exec_ctx);

        let cancel = CancellationToken::new();
        let node = make_node(serde_json::json!({
            "script": "#{ above: inputs.value > ctx.threshold, mode: ctx.mode }"
        }));

        let mut inputs = Outputs::new();
        inputs.insert("value".into(), Value::I64(150));

        let outputs = handler.execute(&node, inputs, cancel).await.unwrap();
        assert_eq!(outputs.get("above"), Some(&Value::Bool(true)));
        assert_eq!(outputs.get("mode"), Some(&Value::String("fast".into())));
    }

    #[tokio::test]
    async fn ctx_get_and_ctx_has_functions() {
        use crate::execute::blackboard::BlackboardScope;

        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);

        let exec_ctx = Arc::new(ExecutionContext::new());
        {
            let mut bb = exec_ctx.blackboard();
            bb.set("key1".into(), Value::I64(42), BlackboardScope::Global);
        }
        handler.set_context(exec_ctx);

        let cancel = CancellationToken::new();
        let node = make_node(serde_json::json!({
            "script": "#{ val: ctx_get(ctx, \"key1\"), exists: ctx_has(ctx, \"key1\"), missing: ctx_has(ctx, \"nope\") }"
        }));

        let outputs = handler
            .execute(&node, Outputs::new(), cancel)
            .await
            .unwrap();
        assert_eq!(outputs.get("val"), Some(&Value::I64(42)));
        assert_eq!(outputs.get("exists"), Some(&Value::Bool(true)));
        assert_eq!(outputs.get("missing"), Some(&Value::Bool(false)));
    }

    #[tokio::test]
    async fn ctx_empty_without_context() {
        let engine = default_script_engine();
        let handler = RhaiHandler::new(engine);
        // No set_context call — ctx should be empty

        let cancel = CancellationToken::new();
        let node = make_node(serde_json::json!({
            "script": "#{ has_key: ctx_has(ctx, \"anything\") }"
        }));

        let outputs = handler
            .execute(&node, Outputs::new(), cancel)
            .await
            .unwrap();
        assert_eq!(outputs.get("has_key"), Some(&Value::Bool(false)));
    }
}
