pub mod anthropic_api;
pub mod claude_cli;
pub mod conversation;
pub mod mock;
pub mod registry;

pub use anthropic_api::AnthropicApiAdapter;
pub use claude_cli::ClaudeCliAdapter;
pub use conversation::{
    ConversationConfig, ConversationHistory, ConversationMessage, MessageRole,
    CONVERSATION_HISTORY_KEY,
};
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

/// Prompt-cache control marker placed on a content block.
///
/// Maps to the Anthropic Messages API `cache_control` field. Adapters that
/// do not support prompt caching ignore this.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheControl {
    /// No cache marker — do not cache this block.
    #[default]
    None,
    /// Ephemeral cache with default TTL (5 minutes on Anthropic).
    Ephemeral,
    /// Ephemeral cache with extended TTL (e.g. "1h"). Requires the
    /// corresponding beta header at the adapter level.
    EphemeralWithTtl {
        /// TTL string accepted by the backend (e.g. "1h").
        ttl: String,
    },
}

impl CacheControl {
    /// True when this control requests no caching.
    pub fn is_none(&self) -> bool {
        matches!(self, CacheControl::None)
    }
}

fn cache_control_is_none(cc: &CacheControl) -> bool {
    cc.is_none()
}

/// A single content block inside a prompt or system message.
///
/// When `cache_control` is not `None`, the adapter is expected to place a
/// cache breakpoint at this block. Content up to and including this block
/// becomes cacheable on supported backends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptBlock {
    pub text: String,
    #[serde(default, skip_serializing_if = "cache_control_is_none")]
    pub cache_control: CacheControl,
}

impl PromptBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cache_control: CacheControl::None,
        }
    }

    pub fn cached(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cache_control: CacheControl::Ephemeral,
        }
    }

    pub fn cached_with_ttl(text: impl Into<String>, ttl: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            cache_control: CacheControl::EphemeralWithTtl { ttl: ttl.into() },
        }
    }
}

/// Request sent to an AI adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiRequest {
    /// The rendered prompt text.
    ///
    /// When `prompt_blocks` is set, that structured representation takes
    /// precedence and this flat text is used only as a fallback for adapters
    /// that don't understand content blocks.
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
    /// Conversation history for stateless adapters.
    /// Assembled from prior LLM interactions on the ancestor path.
    /// Stateful adapters (e.g., Claude CLI in `continue` mode) may ignore this.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversation_history: Vec<conversation::ConversationMessage>,
    /// Optional structured user-turn content. When present, replaces `prompt`
    /// for cache-aware adapters. Each block may carry a `cache_control`
    /// marker indicating a prompt-cache breakpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_blocks: Option<Vec<PromptBlock>>,
    /// Optional system message, as structured blocks. When present, cache-
    /// aware adapters emit this as the top-level `system` field with
    /// per-block cache_control markers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<PromptBlock>>,
    /// When non-`None`, cache-aware adapters mark the last message in
    /// `conversation_history` with this cache_control. Used to cache an
    /// entire prior-turn history as a single ephemeral prefix.
    #[serde(default, skip_serializing_if = "cache_control_is_none")]
    pub conversation_cache_control: CacheControl,
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
            conversation_history: Vec::new(),
            prompt_blocks: None,
            system: None,
            conversation_cache_control: CacheControl::None,
        }
    }

    /// Populate `prompt_blocks` with a cached prefix + an uncached suffix,
    /// and set the flat `prompt` to the concatenation for fallback adapters.
    ///
    /// Cache-aware adapters emit a `cache_control: ephemeral` marker on the
    /// prefix block. Adapters that ignore `prompt_blocks` see the original
    /// concatenated text via `self.prompt`.
    pub fn with_cached_prefix(
        mut self,
        prefix: impl Into<String>,
        suffix: impl Into<String>,
    ) -> Self {
        let prefix = prefix.into();
        let suffix = suffix.into();
        self.prompt = format!("{prefix}{suffix}");
        self.prompt_blocks = Some(vec![
            PromptBlock::cached(prefix),
            PromptBlock::text(suffix),
        ]);
        self
    }

    /// Set structured prompt blocks. Consumers that set `prompt_blocks`
    /// should also ensure `prompt` contains an equivalent concatenation so
    /// non-cache-aware adapters stay functional.
    pub fn with_prompt_blocks(mut self, blocks: Vec<PromptBlock>) -> Self {
        self.prompt_blocks = Some(blocks);
        self
    }

    /// Set the system message as structured blocks.
    pub fn with_system_blocks(mut self, blocks: Vec<PromptBlock>) -> Self {
        self.system = Some(blocks);
        self
    }

    /// Mark the conversation history's last message as a cache breakpoint.
    pub fn with_conversation_cache(mut self, control: CacheControl) -> Self {
        self.conversation_cache_control = control;
        self
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
    /// Tokens served from the prompt cache (Anthropic's
    /// `cache_read_input_tokens`). `None` when the adapter doesn't report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<usize>,
    /// Tokens written into the prompt cache on this call (Anthropic's
    /// `cache_creation_input_tokens`). `None` when the adapter doesn't
    /// report it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<usize>,
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
    fn prompt_block_cached_sets_ephemeral() {
        let block = PromptBlock::cached("prefix");
        assert_eq!(block.cache_control, CacheControl::Ephemeral);
    }

    #[test]
    fn prompt_block_text_has_no_cache() {
        let block = PromptBlock::text("body");
        assert!(block.cache_control.is_none());
    }

    #[test]
    fn prompt_block_cached_with_ttl() {
        let block = PromptBlock::cached_with_ttl("prefix", "1h");
        assert_eq!(
            block.cache_control,
            CacheControl::EphemeralWithTtl { ttl: "1h".into() }
        );
    }

    #[test]
    fn ai_request_with_cached_prefix_populates_blocks_and_flat_prompt() {
        let req = AiRequest::new("").with_cached_prefix("PREFIX", "SUFFIX");
        assert_eq!(req.prompt, "PREFIXSUFFIX");
        let blocks = req.prompt_blocks.as_ref().expect("prompt_blocks set");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "PREFIX");
        assert_eq!(blocks[0].cache_control, CacheControl::Ephemeral);
        assert_eq!(blocks[1].text, "SUFFIX");
        assert!(blocks[1].cache_control.is_none());
    }

    #[test]
    fn ai_request_with_conversation_cache() {
        let req = AiRequest::new("hi").with_conversation_cache(CacheControl::Ephemeral);
        assert_eq!(req.conversation_cache_control, CacheControl::Ephemeral);
    }

    #[test]
    fn ai_request_serde_round_trip_preserves_cache_fields() {
        let req = AiRequest::new("")
            .with_cached_prefix("big static prefix", "dynamic tail")
            .with_conversation_cache(CacheControl::EphemeralWithTtl { ttl: "1h".into() });
        let json = serde_json::to_string(&req).unwrap();
        let parsed: AiRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.prompt, "big static prefixdynamic tail");
        assert_eq!(parsed.prompt_blocks.as_ref().unwrap().len(), 2);
        assert_eq!(
            parsed.conversation_cache_control,
            CacheControl::EphemeralWithTtl { ttl: "1h".into() }
        );
    }

    #[test]
    fn ai_request_backwards_compatible_without_cache_fields() {
        // An old-shape JSON (no prompt_blocks/system/conversation_cache_control)
        // must still deserialize.
        let json = r#"{"prompt":"hi"}"#;
        let parsed: AiRequest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.prompt, "hi");
        assert!(parsed.prompt_blocks.is_none());
        assert!(parsed.system.is_none());
        assert!(parsed.conversation_cache_control.is_none());
    }

    #[test]
    fn token_usage_cache_fields_default_none() {
        let usage = TokenUsage::default();
        assert!(usage.cache_read_input_tokens.is_none());
        assert!(usage.cache_creation_input_tokens.is_none());
    }

    #[test]
    fn token_usage_serde_preserves_cache_fields() {
        let usage = TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_input_tokens: Some(5000),
            cache_creation_input_tokens: Some(0),
        };
        let json = serde_json::to_string(&usage).unwrap();
        let parsed: TokenUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.cache_read_input_tokens, Some(5000));
        assert_eq!(parsed.cache_creation_input_tokens, Some(0));
    }

    #[test]
    fn ai_response_serde_round_trip() {
        let resp = AiResponse {
            text: "response text".into(),
            structured: Some(serde_json::json!({"key": "value"})),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            },
            latency_ms: 150,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: AiResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.text, "response text");
        assert_eq!(parsed.usage.input_tokens, 10);
    }
}
