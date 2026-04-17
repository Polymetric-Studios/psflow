//! JSON transformation handler — extract and shape JSON values via JMESPath.
//!
//! Accepts a JSON value as an input (or parses one from a string input) and
//! applies a JMESPath expression to it. Useful in `http -> json_transform`
//! chains where you want to narrow an API response down to a single field
//! before handing it to a downstream handler.

use crate::error::NodeError;
use crate::execute::{CancellationToken, HandlerSchema, NodeHandler, Outputs, SchemaField};
use crate::graph::node::Node;
use crate::graph::types::Value;
use std::future::Future;
use std::pin::Pin;

/// Handler that runs a JMESPath query over a JSON input.
///
/// ## Configuration
///
/// - `config.query` (required): JMESPath expression, e.g.
///   `"items[?price < `100`].name"`.
/// - `config.source_input`: name of the input port carrying the JSON value
///   (default: `"body"` — matches `http`'s `body` output).
/// - `config.parse_string`: if true, parse the source input as a JSON string
///   before querying (default: true — tolerates `http`'s string `body`).
///
/// ## Outputs
///
/// - `result`: the JMESPath result, converted back to a psflow [`Value`].
///   JMESPath queries that select nothing return `Value::Null`.
pub struct JsonTransformHandler;

impl NodeHandler for JsonTransformHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let config = node.config.clone();
        let node_id = node.id.0.clone();

        Box::pin(async move {
            let query = config
                .get("query")
                .and_then(|v| v.as_str())
                .ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': missing config.query"),
                    recoverable: false,
                })?;

            let source_key = config
                .get("source_input")
                .and_then(|v| v.as_str())
                .unwrap_or("body");

            let parse_string = config
                .get("parse_string")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            let source_value = inputs.get(source_key).ok_or_else(|| NodeError::Failed {
                source_message: None,
                message: format!(
                    "node '{node_id}': missing input '{source_key}' for json_transform"
                ),
                recoverable: false,
            })?;

            let json: serde_json::Value = match source_value {
                Value::String(s) if parse_string => {
                    serde_json::from_str(s).map_err(|e| NodeError::Failed {
                        source_message: Some(e.to_string()),
                        message: format!(
                            "node '{node_id}': failed to parse input '{source_key}' as JSON: {e}"
                        ),
                        recoverable: false,
                    })?
                }
                other => serde_json::Value::from(other),
            };

            let expr = jmespath::compile(query).map_err(|e| NodeError::Failed {
                source_message: Some(e.to_string()),
                message: format!("node '{node_id}': invalid JMESPath query '{query}': {e}"),
                recoverable: false,
            })?;

            let runtime_value = jmespath::Variable::from_json(&json.to_string()).map_err(|e| {
                NodeError::Failed {
                    source_message: Some(e.to_string()),
                    message: format!("node '{node_id}': failed to load JSON for JMESPath: {e}"),
                    recoverable: false,
                }
            })?;

            let result = expr.search(runtime_value).map_err(|e| NodeError::Failed {
                source_message: Some(e.to_string()),
                message: format!("node '{node_id}': JMESPath search failed: {e}"),
                recoverable: false,
            })?;

            let rendered = result.to_string();
            let result_json: serde_json::Value =
                serde_json::from_str(&rendered).unwrap_or(serde_json::Value::Null);
            let psflow_value = Value::from(result_json);

            let mut outputs = Outputs::new();
            outputs.insert("result".into(), psflow_value);
            Ok(outputs)
        })
    }

    fn schema(&self, name: &str) -> HandlerSchema {
        HandlerSchema::new(name, "Extract or shape JSON using JMESPath")
            .with_config(
                SchemaField::new("query", "string")
                    .required()
                    .describe("JMESPath expression"),
            )
            .with_config(
                SchemaField::new("source_input", "string")
                    .describe("Input port carrying the JSON source")
                    .default(serde_json::json!("body")),
            )
            .with_config(
                SchemaField::new("parse_string", "boolean")
                    .describe("Parse source as JSON string before querying")
                    .default(serde_json::json!(true)),
            )
            .with_output(SchemaField::new("result", "any"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_with(config: serde_json::Value) -> Node {
        let mut n = Node::new("jt", "JsonTransform").with_handler("json_transform");
        n.config = config;
        n
    }

    fn inputs_with_body(body: &str) -> Outputs {
        let mut i = Outputs::new();
        i.insert("body".into(), Value::String(body.to_string()));
        i
    }

    #[tokio::test]
    async fn extracts_simple_field() {
        let node = node_with(serde_json::json!({ "query": "name" }));
        let inputs = inputs_with_body(r#"{"name":"alice","age":42}"#);
        let out = JsonTransformHandler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.get("result"), Some(&Value::String("alice".into())));
    }

    #[tokio::test]
    async fn extracts_nested_path() {
        let node = node_with(serde_json::json!({ "query": "user.email" }));
        let inputs = inputs_with_body(r#"{"user":{"email":"a@b.co"}}"#);
        let out = JsonTransformHandler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.get("result"), Some(&Value::String("a@b.co".into())));
    }

    #[tokio::test]
    async fn filters_array() {
        let node = node_with(serde_json::json!({ "query": "items[?done].name" }));
        let inputs = inputs_with_body(
            r#"{"items":[
                {"name":"a","done":true},
                {"name":"b","done":false},
                {"name":"c","done":true}
            ]}"#,
        );
        let out = JsonTransformHandler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();
        let result = out.get("result").unwrap();
        match result {
            Value::Vec(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Value::String("a".into()));
                assert_eq!(items[1], Value::String("c".into()));
            }
            other => panic!("expected Vec, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_query_errors() {
        let node = node_with(serde_json::json!({}));
        let inputs = inputs_with_body("{}");
        let result = JsonTransformHandler
            .execute(&node, inputs, CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing config.query"));
    }

    #[tokio::test]
    async fn invalid_query_errors() {
        let node = node_with(serde_json::json!({ "query": "{{broken" }));
        let inputs = inputs_with_body("{}");
        let result = JsonTransformHandler
            .execute(&node, inputs, CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid JMESPath"));
    }

    #[tokio::test]
    async fn missing_source_input_errors() {
        let node = node_with(serde_json::json!({ "query": "x" }));
        let result = JsonTransformHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing input"));
    }

    #[tokio::test]
    async fn custom_source_input_name() {
        let node = node_with(serde_json::json!({ "query": "v", "source_input": "payload" }));
        let mut inputs = Outputs::new();
        inputs.insert("payload".into(), Value::String(r#"{"v":7}"#.into()));
        let out = JsonTransformHandler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.get("result"), Some(&Value::I64(7)));
    }

    #[tokio::test]
    async fn no_match_returns_null() {
        let node = node_with(serde_json::json!({ "query": "missing" }));
        let inputs = inputs_with_body(r#"{"name":"x"}"#);
        let out = JsonTransformHandler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.get("result"), Some(&Value::Null));
    }

    #[tokio::test]
    async fn parse_string_false_takes_value_directly() {
        // When input is already a psflow Map, skip JSON parsing.
        let node = node_with(serde_json::json!({
            "query": "name",
            "parse_string": false,
        }));
        let mut map = std::collections::BTreeMap::new();
        map.insert("name".into(), Value::String("bob".into()));
        let mut inputs = Outputs::new();
        inputs.insert("body".into(), Value::Map(map));
        let out = JsonTransformHandler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.get("result"), Some(&Value::String("bob".into())));
    }
}
