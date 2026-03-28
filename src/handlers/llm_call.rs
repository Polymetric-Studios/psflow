use crate::adapter::{AiAdapter, AiRequest};
use crate::error::NodeError;
use crate::execute::{CancellationToken, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::template::PromptTemplate;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Node handler that delegates to an AI adapter via a prompt template.
///
/// Supports two modes:
/// - **Transform:** data flows in, LLM processes it, structured data flows out.
///   The response text (or structured JSON) is placed in the `"response"` output port.
/// - **Oracle:** the LLM makes a routing decision. The response is placed in
///   `"decision"` output port for downstream branch guards to evaluate.
///
/// Configuration (from node's `config` JSON):
/// - `prompt` (required): Template string with `{inputs.*}` / `{ctx.*}` placeholders.
/// - `adapter`: Adapter name override (uses default if absent).
/// - `model`: Model override passed to the adapter.
/// - `temperature`: Sampling temperature.
/// - `max_tokens`: Maximum tokens in response.
/// - `output_format`: `"text"` (default) or `"json"`.
/// - `mode`: `"transform"` (default) or `"oracle"`.
pub struct LlmCallHandler {
    adapter: Arc<dyn AiAdapter>,
}

impl LlmCallHandler {
    pub fn new(adapter: Arc<dyn AiAdapter>) -> Self {
        Self { adapter }
    }
}

impl NodeHandler for LlmCallHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let adapter = self.adapter.clone();
        let config = node.config.clone();
        let node_id = node.id.0.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before LLM call".into(),
                });
            }

            // Extract prompt template from config
            let prompt_str = config
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': missing config.prompt"),
                    recoverable: false,
                })?;

            // Compile and render the prompt template
            let template = PromptTemplate::compile(prompt_str).map_err(|e| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': template error: {e}"),
                recoverable: false,
            })?;

            // Use an empty blackboard for rendering — the handler doesn't have
            // direct blackboard access in the current trait signature.
            let blackboard = crate::execute::blackboard::Blackboard::new();
            let rendered = template.render(&inputs, &blackboard).map_err(|e| {
                NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': template render error: {e}"),
                    recoverable: false,
                }
            })?;

            // Build the AI request
            let mut req = AiRequest::new(rendered);

            if let Some(temp) = config.get("temperature").and_then(|v| v.as_f64()) {
                req.temperature = Some(temp as f32);
            }
            if let Some(tokens) = config.get("max_tokens").and_then(|v| v.as_u64()) {
                req.max_tokens = Some(tokens as usize);
            }
            if let Some(model) = config.get("model").and_then(|v| v.as_str()) {
                req.model = Some(model.to_string());
            }

            let output_format = config
                .get("output_format")
                .and_then(|v| v.as_str())
                .unwrap_or("text");

            if output_format == "json" {
                req.output_schema = Some(serde_json::json!({"type": "object"}));
            }

            // Check cancellation before the potentially long LLM call
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before LLM call".into(),
                });
            }

            // Make the adapter call
            let response = adapter.complete(req).await?;

            // Build outputs
            let mut outputs = Outputs::new();
            let mode = config
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("transform");

            let output_key = if mode == "oracle" {
                "decision"
            } else {
                "response"
            };

            // If structured output is available, use it; otherwise use text
            if let Some(structured) = response.structured {
                outputs.insert(
                    output_key.into(),
                    Value::Domain {
                        type_name: "json".into(),
                        data: structured,
                    },
                );
            } else {
                outputs.insert(output_key.into(), Value::String(response.text));
            }

            // Always include usage metadata
            outputs.insert(
                "_usage_input_tokens".into(),
                Value::I64(response.usage.input_tokens as i64),
            );
            outputs.insert(
                "_usage_output_tokens".into(),
                Value::I64(response.usage.output_tokens as i64),
            );
            outputs.insert(
                "_latency_ms".into(),
                Value::I64(response.latency_ms as i64),
            );

            Ok(outputs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::mock::MockAdapter;
    use crate::graph::node::Node;

    #[tokio::test]
    async fn transform_mode_returns_response() {
        let adapter = Arc::new(
            MockAdapter::new().with_response("summarize", "A brief summary."),
        );
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM1", "Summarize");
        node.config = serde_json::json!({
            "prompt": "Please summarize: {inputs.text}"
        });

        let mut inputs = Outputs::new();
        inputs.insert("text".into(), Value::String("Long article...".into()));

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            result.get("response"),
            Some(&Value::String("A brief summary.".into()))
        );
        assert!(result.contains_key("_latency_ms"));
    }

    #[tokio::test]
    async fn oracle_mode_returns_decision() {
        let adapter = Arc::new(MockAdapter::new().with_response("classify", "tech"));
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM2", "Classify");
        node.config = serde_json::json!({
            "prompt": "classify this: {inputs.text}",
            "mode": "oracle"
        });

        let mut inputs = Outputs::new();
        inputs.insert("text".into(), Value::String("AI news".into()));

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            result.get("decision"),
            Some(&Value::String("tech".into()))
        );
    }

    #[tokio::test]
    async fn json_output_format() {
        let adapter = Arc::new(
            MockAdapter::new().with_response("analyze", r#"{"score": 0.9}"#),
        );
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM3", "Analyze");
        node.config = serde_json::json!({
            "prompt": "analyze: {inputs.data}",
            "output_format": "json"
        });

        let mut inputs = Outputs::new();
        inputs.insert("data".into(), Value::String("test data".into()));

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        match result.get("response") {
            Some(Value::Domain { type_name, data }) => {
                assert_eq!(type_name, "json");
                assert_eq!(data["score"], 0.9);
            }
            other => panic!("expected Domain(json), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_prompt_is_error() {
        let adapter = Arc::new(MockAdapter::new());
        let handler = LlmCallHandler::new(adapter);

        let node = Node::new("LLM4", "NoPrompt");
        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancellation_before_call() {
        let adapter = Arc::new(MockAdapter::new());
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM5", "Cancelled");
        node.config = serde_json::json!({ "prompt": "test" });

        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = handler
            .execute(&node, Outputs::new(), cancel)
            .await;
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    #[tokio::test]
    async fn config_passes_through_to_request() {
        let adapter = Arc::new(MockAdapter::new().with_default("ok"));
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM6", "Configured");
        node.config = serde_json::json!({
            "prompt": "test",
            "temperature": 0.3,
            "max_tokens": 500,
            "model": "claude-sonnet"
        });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        // The mock adapter doesn't validate these, but the handler should
        // produce a valid response
        assert!(result.contains_key("response"));
    }
}
