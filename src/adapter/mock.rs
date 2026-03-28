use crate::adapter::{AdapterCapabilities, AiAdapter, AiRequest, AiResponse, TokenUsage};
use crate::error::NodeError;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

/// A deterministic AI adapter for testing and CI.
///
/// Maps prompt patterns to canned responses. When a prompt contains a
/// registered pattern (substring match), the first matching response
/// (in registration order) is returned. If no pattern matches, a
/// configurable default response is used.
pub struct MockAdapter {
    responses: Vec<(String, String)>,
    default_response: String,
    capabilities: AdapterCapabilities,
}

impl MockAdapter {
    pub fn new() -> Self {
        Self {
            responses: Vec::new(),
            default_response: "mock response".into(),
            capabilities: AdapterCapabilities {
                tool_use: true,
                structured_output: true,
                vision: false,
                conversation_history: true,
                max_tokens: Some(100_000),
            },
        }
    }

    /// Add a pattern → response mapping.
    /// When a prompt contains `pattern` as a substring, `response` is returned.
    /// Add a pattern → response mapping (first-registered-wins on overlap).
    pub fn with_response(mut self, pattern: impl Into<String>, response: impl Into<String>) -> Self {
        self.responses.push((pattern.into(), response.into()));
        self
    }

    /// Set the default response when no pattern matches.
    pub fn with_default(mut self, response: impl Into<String>) -> Self {
        self.default_response = response.into();
        self
    }

    /// Override capabilities for testing capability validation.
    pub fn with_capabilities(mut self, caps: AdapterCapabilities) -> Self {
        self.capabilities = caps;
        self
    }

    fn find_response(&self, prompt: &str) -> String {
        for (pattern, response) in &self.responses {
            if prompt.contains(pattern.as_str()) {
                return response.clone();
            }
        }
        self.default_response.clone()
    }
}

impl Default for MockAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AiAdapter for MockAdapter {
    fn complete(
        &self,
        req: AiRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AiResponse, NodeError>> + Send + '_>> {
        let start = Instant::now();
        let text = self.find_response(&req.prompt);
        let structured = req.output_schema.as_ref().map(|_| {
            // Try to parse the response as JSON for structured output
            serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text.clone()))
        });

        Box::pin(async move {
            Ok(AiResponse {
                text,
                structured,
                usage: TokenUsage {
                    input_tokens: req.prompt.len() / 4, // rough estimate
                    output_tokens: 10,
                },
                latency_ms: start.elapsed().as_millis() as u64,
            })
        })
    }

    fn judge(
        &self,
        candidates: &[String],
        _criteria: &str,
    ) -> Pin<Box<dyn Future<Output = Result<usize, NodeError>> + Send + '_>> {
        // Mock: always pick the first candidate
        let result = if candidates.is_empty() {
            Err(NodeError::Failed {
                source_message: None,
                message: "no candidates to judge".into(),
                recoverable: false,
            })
        } else {
            Ok(0)
        };
        Box::pin(async move { result })
    }

    fn capabilities(&self) -> &AdapterCapabilities {
        &self.capabilities
    }

    fn name(&self) -> &str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn complete_returns_matched_response() {
        let adapter = MockAdapter::new()
            .with_response("classify", "category: tech")
            .with_response("summarize", "This is a summary.");

        let resp = adapter
            .complete(AiRequest::new("Please classify this article"))
            .await
            .unwrap();
        assert_eq!(resp.text, "category: tech");
    }

    #[tokio::test]
    async fn complete_returns_default_when_no_match() {
        let adapter = MockAdapter::new().with_default("fallback");

        let resp = adapter
            .complete(AiRequest::new("unknown prompt"))
            .await
            .unwrap();
        assert_eq!(resp.text, "fallback");
    }

    #[tokio::test]
    async fn complete_with_structured_output() {
        let adapter = MockAdapter::new()
            .with_response("analyze", r#"{"score": 0.95}"#);

        let req = AiRequest {
            prompt: "analyze this".into(),
            output_schema: Some(serde_json::json!({"type": "object"})),
            ..AiRequest::new("")
        };
        let resp = adapter.complete(req).await.unwrap();
        assert!(resp.structured.is_some());
        assert_eq!(resp.structured.unwrap()["score"], 0.95);
    }

    #[tokio::test]
    async fn judge_returns_first_candidate() {
        let adapter = MockAdapter::new();
        let idx = adapter
            .judge(&["a".into(), "b".into()], "pick best")
            .await
            .unwrap();
        assert_eq!(idx, 0);
    }

    #[tokio::test]
    async fn judge_empty_candidates_is_error() {
        let adapter = MockAdapter::new();
        assert!(adapter.judge(&[], "criteria").await.is_err());
    }

    #[test]
    fn capabilities_reflect_configuration() {
        let adapter = MockAdapter::new().with_capabilities(AdapterCapabilities {
            vision: true,
            ..Default::default()
        });
        assert!(adapter.capabilities().vision);
        assert!(!adapter.capabilities().tool_use);
    }

    #[test]
    fn name_is_mock() {
        assert_eq!(MockAdapter::new().name(), "mock");
    }
}
