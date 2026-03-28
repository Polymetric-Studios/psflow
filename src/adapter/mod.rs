pub mod mock;
pub mod registry;

pub use mock::MockAdapter;
pub use registry::AdapterRegistry;

use crate::error::NodeError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

/// Capabilities declared by an AI adapter.
///
/// Nodes can declare required capabilities; graph validation rejects
/// mismatches before execution starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdapterCapabilities {
    pub tool_use: bool,
    pub structured_output: bool,
    pub vision: bool,
    pub conversation_history: bool,
    pub max_tokens: Option<usize>,
}

impl AdapterCapabilities {
    /// Check whether this adapter satisfies all the given requirements.
    pub fn satisfies(&self, required: &AdapterCapabilities) -> bool {
        (!required.tool_use || self.tool_use)
            && (!required.structured_output || self.structured_output)
            && (!required.vision || self.vision)
            && (!required.conversation_history || self.conversation_history)
            && match (required.max_tokens, self.max_tokens) {
                (Some(req), Some(cap)) => cap >= req,
                (Some(_), None) => false,
                _ => true,
            }
    }

    /// Human-readable list of capabilities that `required` asks for but this adapter lacks.
    pub fn missing(&self, required: &AdapterCapabilities) -> Vec<&'static str> {
        let mut gaps = Vec::new();
        if required.tool_use && !self.tool_use {
            gaps.push("tool_use");
        }
        if required.structured_output && !self.structured_output {
            gaps.push("structured_output");
        }
        if required.vision && !self.vision {
            gaps.push("vision");
        }
        if required.conversation_history && !self.conversation_history {
            gaps.push("conversation_history");
        }
        if let (Some(req), Some(cap)) = (required.max_tokens, self.max_tokens) {
            if cap < req {
                gaps.push("max_tokens");
            }
        } else if required.max_tokens.is_some() && self.max_tokens.is_none() {
            gaps.push("max_tokens");
        }
        gaps
    }
}

/// Request sent to an AI adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiRequest {
    /// The rendered prompt text.
    pub prompt: String,
    /// Variables available for reference (already interpolated into prompt).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub variables: HashMap<String, String>,
    /// Optional JSON Schema for structured output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// Optional model override (adapter-specific).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl AiRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            variables: HashMap::new(),
            output_schema: None,
            temperature: None,
            max_tokens: None,
            stop_sequences: Vec::new(),
            model: None,
        }
    }

    pub fn with_temperature(mut self, temp: f32) -> Self {
        self.temperature = Some(temp);
        self
    }

    pub fn with_max_tokens(mut self, tokens: usize) -> Self {
        self.max_tokens = Some(tokens);
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// Response from an AI adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiResponse {
    /// The raw text response.
    pub text: String,
    /// Parsed structured data (if output_schema was provided and adapter supports it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
    /// Token usage statistics.
    #[serde(default)]
    pub usage: TokenUsage,
    /// Response latency in milliseconds.
    pub latency_ms: u64,
}

/// Token usage statistics from an AI call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
}

/// Core trait for AI backends.
///
/// All LLM integrations (Claude API, OpenAI-compatible, local models, mock)
/// implement this trait. The adapter is selected per-graph or per-node via
/// annotation and the `AdapterRegistry`.
pub trait AiAdapter: Send + Sync {
    /// Send a completion request and return the response.
    fn complete(
        &self,
        req: AiRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AiResponse, NodeError>> + Send + '_>>;

    /// Judge/rank candidates against criteria. Returns the index of the winner.
    fn judge(
        &self,
        candidates: &[String],
        criteria: &str,
    ) -> Pin<Box<dyn Future<Output = Result<usize, NodeError>> + Send + '_>>;

    /// Declare this adapter's capabilities for validation.
    fn capabilities(&self) -> &AdapterCapabilities;

    /// Human-readable adapter name (e.g., "mock", "claude_cli", "anthropic_api").
    fn name(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_satisfies_exact_match() {
        let cap = AdapterCapabilities {
            tool_use: true,
            structured_output: true,
            vision: false,
            conversation_history: true,
            max_tokens: Some(100_000),
        };
        let req = AdapterCapabilities {
            tool_use: true,
            structured_output: true,
            ..Default::default()
        };
        assert!(cap.satisfies(&req));
    }

    #[test]
    fn capabilities_missing_tool_use() {
        let cap = AdapterCapabilities::default();
        let req = AdapterCapabilities {
            tool_use: true,
            ..Default::default()
        };
        assert!(!cap.satisfies(&req));
        assert_eq!(cap.missing(&req), vec!["tool_use"]);
    }

    #[test]
    fn capabilities_max_tokens_insufficient() {
        let cap = AdapterCapabilities {
            max_tokens: Some(4096),
            ..Default::default()
        };
        let req = AdapterCapabilities {
            max_tokens: Some(100_000),
            ..Default::default()
        };
        assert!(!cap.satisfies(&req));
    }

    #[test]
    fn capabilities_no_requirements_always_satisfies() {
        let cap = AdapterCapabilities::default();
        let req = AdapterCapabilities::default();
        assert!(cap.satisfies(&req));
    }

    #[test]
    fn ai_request_builder() {
        let req = AiRequest::new("Hello")
            .with_temperature(0.7)
            .with_max_tokens(500)
            .with_model("claude-sonnet");
        assert_eq!(req.prompt, "Hello");
        assert_eq!(req.temperature, Some(0.7));
        assert_eq!(req.max_tokens, Some(500));
        assert_eq!(req.model, Some("claude-sonnet".into()));
    }

    #[test]
    fn ai_request_serde_round_trip() {
        let req = AiRequest::new("test prompt").with_temperature(0.5);
        let json = serde_json::to_string(&req).unwrap();
        let parsed: AiRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.prompt, "test prompt");
        assert_eq!(parsed.temperature, Some(0.5));
    }

    #[test]
    fn ai_response_serde_round_trip() {
        let resp = AiResponse {
            text: "response text".into(),
            structured: Some(serde_json::json!({"key": "value"})),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
            },
            latency_ms: 150,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: AiResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.text, "response text");
        assert_eq!(parsed.usage.input_tokens, 10);
    }
}
