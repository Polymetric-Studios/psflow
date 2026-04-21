//! Direct HTTP adapter for the Anthropic Messages API (`/v1/messages`).
//!
//! Supports prompt caching via `cache_control` markers translated from
//! `AiRequest::prompt_blocks`, `AiRequest::system`, and
//! `AiRequest::conversation_cache_control`.
//!
//! Non-streaming. Streaming, tool use, and image content are deferred.

use crate::adapter::{
    AdapterCapabilities, AiAdapter, AiRequest, AiResponse, CacheControl, MessageRole, PromptBlock,
    TokenUsage,
};
use crate::error::NodeError;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const ANTHROPIC_VERSION: &str = "2023-06-01";
const EXTENDED_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";
const ADAPTER_NAME: &str = "anthropic_api";

/// Direct Anthropic Messages API adapter.
///
/// Reads the API key from the `ANTHROPIC_API_KEY` environment variable by
/// default. Use `with_api_key` for explicit override (e.g., tests).
///
/// The `Debug` impl redacts the API key.
pub struct AnthropicApiAdapter {
    api_key: String,
    default_model: Option<String>,
    client: Client,
    base_url: String,
    capabilities: AdapterCapabilities,
}

impl AnthropicApiAdapter {
    /// Construct using `ANTHROPIC_API_KEY` from the environment.
    ///
    /// Errors when the env var is unset or empty — this is a hard
    /// configuration error, not something callers should recover from at
    /// runtime.
    pub fn from_env() -> Result<Self, NodeError> {
        let key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| NodeError::AdapterError {
                adapter: ADAPTER_NAME.into(),
                message: "ANTHROPIC_API_KEY is not set".into(),
            })?;
        Ok(Self::with_api_key(key))
    }

    /// Construct with an explicit API key. Useful for tests.
    pub fn with_api_key(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            default_model: None,
            client: Client::new(),
            base_url: DEFAULT_BASE_URL.into(),
            capabilities: AdapterCapabilities {
                tool_use: false,
                structured_output: true,
                vision: false,
                conversation_history: true,
                max_tokens: Some(200_000),
            },
        }
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Override the base URL. Primarily used by tests that target a local
    /// mock server. Include scheme + host, no trailing slash.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override capabilities (e.g. to match a specific target model).
    pub fn with_capabilities(mut self, caps: AdapterCapabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// Inspect the redacted form of the api key for debugging.
    fn redacted_key(&self) -> String {
        if self.api_key.len() <= 4 {
            "<redacted>".into()
        } else {
            format!("<redacted:len={}>", self.api_key.len())
        }
    }
}

impl std::fmt::Debug for AnthropicApiAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicApiAdapter")
            .field("api_key", &self.redacted_key())
            .field("default_model", &self.default_model)
            .field("base_url", &self.base_url)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AnthropicMessagesRequest<'a> {
    model: &'a str,
    max_tokens: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<AnthropicTextBlock<'a>>>,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    stop_sequences: &'a [String],
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: Vec<AnthropicTextBlock<'a>>,
}

#[derive(Serialize)]
struct AnthropicTextBlock<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<AnthropicCacheControl>,
}

#[derive(Serialize)]
struct AnthropicCacheControl {
    #[serde(rename = "type")]
    ty: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<String>,
}

impl AnthropicCacheControl {
    fn from_cache_control(cc: &CacheControl) -> Option<Self> {
        match cc {
            CacheControl::None => None,
            CacheControl::Ephemeral => Some(Self {
                ty: "ephemeral",
                ttl: None,
            }),
            CacheControl::EphemeralWithTtl { ttl } => Some(Self {
                ty: "ephemeral",
                ttl: Some(ttl.clone()),
            }),
        }
    }
}

#[derive(Deserialize)]
struct AnthropicMessagesResponse {
    #[serde(default)]
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    usage: AnthropicUsage,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: usize,
    #[serde(default)]
    output_tokens: usize,
    #[serde(default)]
    cache_creation_input_tokens: Option<usize>,
    #[serde(default)]
    cache_read_input_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct AnthropicErrorBody {
    error: Option<AnthropicErrorDetail>,
}

#[derive(Deserialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    ty: Option<String>,
    message: Option<String>,
}

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

/// Translate an `AiRequest` into the JSON body for `/v1/messages`, plus a
/// flag indicating whether any block uses the extended-TTL form (and thus
/// requires the beta header).
#[derive(Debug)]
struct TranslatedRequest {
    body: serde_json::Value,
    needs_extended_ttl_beta: bool,
}

fn translate_request(
    req: &AiRequest,
    default_model: Option<&str>,
) -> Result<TranslatedRequest, NodeError> {
    let model = req
        .model
        .as_deref()
        .or(default_model)
        .ok_or_else(|| NodeError::AdapterError {
            adapter: ADAPTER_NAME.into(),
            message: "no model specified on request or adapter".into(),
        })?;

    let max_tokens = req.max_tokens.ok_or_else(|| NodeError::AdapterError {
        adapter: ADAPTER_NAME.into(),
        message: "AiRequest.max_tokens is required for anthropic_api".into(),
    })?;

    let mut needs_ext = false;

    // System blocks
    let system = req
        .system
        .as_ref()
        .map(|blocks| build_blocks(blocks, &mut needs_ext));

    // Conversation history → messages, with optional cache marker on last.
    let mut messages: Vec<AnthropicMessage> = Vec::new();
    if !req.conversation_history.is_empty() {
        let last_idx = req.conversation_history.len() - 1;
        for (i, msg) in req.conversation_history.iter().enumerate() {
            let role = match msg.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                // System messages in history map into the top-level system
                // field in Anthropic; but since we only place cache marker
                // on last-assistant here, we route system-role history
                // messages into the user-turn stream as a fallback.
                MessageRole::System => "user",
            };

            let mut cc = CacheControl::None;
            if i == last_idx && !req.conversation_cache_control.is_none() {
                cc = req.conversation_cache_control.clone();
            }
            if matches!(cc, CacheControl::EphemeralWithTtl { .. }) {
                needs_ext = true;
            }

            messages.push(AnthropicMessage {
                role,
                content: vec![AnthropicTextBlock {
                    ty: "text",
                    text: &msg.content,
                    cache_control: AnthropicCacheControl::from_cache_control(&cc),
                }],
            });
        }
    }

    // Final user-turn content: prefer prompt_blocks when present.
    let user_content: Vec<AnthropicTextBlock> = if let Some(blocks) = &req.prompt_blocks {
        build_blocks(blocks, &mut needs_ext)
    } else {
        vec![AnthropicTextBlock {
            ty: "text",
            text: &req.prompt,
            cache_control: None,
        }]
    };
    messages.push(AnthropicMessage {
        role: "user",
        content: user_content,
    });

    let wire = AnthropicMessagesRequest {
        model,
        max_tokens,
        temperature: req.temperature,
        system,
        messages,
        stop_sequences: &req.stop_sequences,
    };

    let body = serde_json::to_value(&wire).map_err(|e| NodeError::AdapterError {
        adapter: ADAPTER_NAME.into(),
        message: format!("failed to serialize request: {e}"),
    })?;

    Ok(TranslatedRequest {
        body,
        needs_extended_ttl_beta: needs_ext,
    })
}

fn build_blocks<'a>(
    blocks: &'a [PromptBlock],
    needs_ext: &mut bool,
) -> Vec<AnthropicTextBlock<'a>> {
    blocks
        .iter()
        .map(|b| {
            if matches!(b.cache_control, CacheControl::EphemeralWithTtl { .. }) {
                *needs_ext = true;
            }
            AnthropicTextBlock {
                ty: "text",
                text: &b.text,
                cache_control: AnthropicCacheControl::from_cache_control(&b.cache_control),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Adapter impl
// ---------------------------------------------------------------------------

impl AiAdapter for AnthropicApiAdapter {
    fn complete(
        &self,
        req: AiRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AiResponse, NodeError>> + Send + '_>> {
        Box::pin(async move {
            let start = Instant::now();

            let translated = translate_request(&req, self.default_model.as_deref())?;

            let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
            let mut builder = self
                .client
                .post(&url)
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json");

            if translated.needs_extended_ttl_beta {
                builder = builder.header("anthropic-beta", EXTENDED_TTL_BETA);
            }

            let resp = builder
                .json(&translated.body)
                .send()
                .await
                .map_err(network_error)?;

            let status = resp.status();
            let headers = resp.headers().clone();
            let body_bytes = resp.bytes().await.map_err(|e| NodeError::AdapterError {
                adapter: ADAPTER_NAME.into(),
                message: format!("failed to read response body: {e}"),
            })?;

            let latency_ms = start.elapsed().as_millis() as u64;

            if !status.is_success() {
                return Err(http_error(status, &headers, &body_bytes));
            }

            let parsed: AnthropicMessagesResponse =
                serde_json::from_slice(&body_bytes).map_err(|e| NodeError::AdapterError {
                    adapter: ADAPTER_NAME.into(),
                    message: format!("failed to parse Anthropic response: {e}"),
                })?;

            let mut text = String::new();
            for block in &parsed.content {
                if let AnthropicContentBlock::Text { text: t } = block {
                    text.push_str(t);
                }
            }

            if let (Some(read), Some(create)) = (
                parsed.usage.cache_read_input_tokens,
                parsed.usage.cache_creation_input_tokens,
            ) {
                tracing::debug!(
                    target: "psflow::adapter::anthropic_api",
                    cache_read = read,
                    cache_create = create,
                    "anthropic_api cache usage"
                );
            }

            let usage = TokenUsage {
                input_tokens: parsed.usage.input_tokens,
                output_tokens: parsed.usage.output_tokens,
                cache_read_input_tokens: parsed.usage.cache_read_input_tokens,
                cache_creation_input_tokens: parsed.usage.cache_creation_input_tokens,
            };

            let structured = if req.output_schema.is_some() {
                serde_json::from_str(&text).ok()
            } else {
                None
            };

            Ok(AiResponse {
                text,
                structured,
                usage,
                latency_ms,
            })
        })
    }

    fn judge(
        &self,
        candidates: &[String],
        criteria: &str,
    ) -> Pin<Box<dyn Future<Output = Result<usize, NodeError>> + Send + '_>> {
        if candidates.is_empty() {
            return Box::pin(async {
                Err(NodeError::Failed {
                    source_message: None,
                    message: "no candidates to judge".into(),
                    recoverable: false,
                })
            });
        }

        let mut prompt =
            format!("Given these candidates, select the best one based on: {criteria}\n\n");
        for (i, c) in candidates.iter().enumerate() {
            prompt.push_str(&format!("Candidate {i}: {c}\n\n"));
        }
        prompt.push_str("Respond with ONLY the candidate number (0-based index). Nothing else.");

        let mut req = AiRequest::new(prompt).with_temperature(0.0);
        if req.max_tokens.is_none() {
            req.max_tokens = Some(16);
        }
        let num = candidates.len();

        Box::pin(async move {
            let resp = self.complete(req).await?;
            let text = resp.text.trim();
            let idx = text.parse::<usize>().map_err(|_| NodeError::AdapterError {
                adapter: ADAPTER_NAME.into(),
                message: format!("judge response was not a valid index: '{text}'"),
            })?;
            if idx >= num {
                return Err(NodeError::AdapterError {
                    adapter: ADAPTER_NAME.into(),
                    message: format!("judge returned index {idx} but only {num} candidates exist"),
                });
            }
            Ok(idx)
        })
    }

    fn capabilities(&self) -> &AdapterCapabilities {
        &self.capabilities
    }

    fn name(&self) -> &str {
        ADAPTER_NAME
    }
}

// ---------------------------------------------------------------------------
// Error translation
// ---------------------------------------------------------------------------

fn network_error(err: reqwest::Error) -> NodeError {
    let recoverable = err.is_timeout() || err.is_connect();
    if recoverable {
        NodeError::Failed {
            source_message: None,
            message: format!("anthropic_api network error: {err}"),
            recoverable: true,
        }
    } else {
        NodeError::AdapterError {
            adapter: ADAPTER_NAME.into(),
            message: format!("network error: {err}"),
        }
    }
}

fn http_error(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
) -> NodeError {
    let server_message = serde_json::from_slice::<AnthropicErrorBody>(body)
        .ok()
        .and_then(|b| b.error)
        .and_then(|e| {
            let ty = e.ty.unwrap_or_default();
            let msg = e.message.unwrap_or_default();
            if ty.is_empty() && msg.is_empty() {
                None
            } else {
                Some(format!("{ty}: {msg}"))
            }
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());

    match status.as_u16() {
        401 | 403 => NodeError::AdapterError {
            adapter: ADAPTER_NAME.into(),
            message: format!("auth error ({status}); check ANTHROPIC_API_KEY: {server_message}"),
        },
        404 => NodeError::AdapterError {
            adapter: ADAPTER_NAME.into(),
            message: format!("not found ({status}): {server_message}"),
        },
        429 => {
            let retry_after = headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            NodeError::Failed {
                source_message: None,
                message: format!(
                    "anthropic_api rate_limit_error ({status}), retry_after={retry_after}: {server_message}"
                ),
                recoverable: true,
            }
        }
        529 => NodeError::Failed {
            source_message: None,
            message: format!("anthropic_api overloaded_error ({status}): {server_message}"),
            recoverable: true,
        },
        code if (500..600).contains(&code) => NodeError::Failed {
            source_message: None,
            message: format!("anthropic_api server error ({status}): {server_message}"),
            recoverable: true,
        },
        _ => NodeError::AdapterError {
            adapter: ADAPTER_NAME.into(),
            message: format!("http {status}: {server_message}"),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::conversation::ConversationMessage;

    fn sample_request() -> AiRequest {
        let mut r = AiRequest::new("hello world").with_max_tokens(128);
        r.model = Some("claude-sonnet-4-5".into());
        r
    }

    #[test]
    fn name_is_anthropic_api() {
        let a = AnthropicApiAdapter::with_api_key("sk-test");
        assert_eq!(a.name(), ADAPTER_NAME);
    }

    #[test]
    fn debug_redacts_api_key() {
        let a = AnthropicApiAdapter::with_api_key("sk-ant-supersecret");
        let s = format!("{a:?}");
        assert!(!s.contains("supersecret"));
        assert!(s.contains("redacted"));
    }

    #[test]
    fn capabilities_defaults() {
        let a = AnthropicApiAdapter::with_api_key("k");
        assert!(!a.capabilities().tool_use);
        assert!(a.capabilities().structured_output);
        assert!(!a.capabilities().vision);
        assert!(a.capabilities().conversation_history);
        assert_eq!(a.capabilities().max_tokens, Some(200_000));
    }

    #[test]
    fn translate_flat_prompt() {
        let req = sample_request();
        let t = translate_request(&req, None).unwrap();
        assert!(!t.needs_extended_ttl_beta);

        let messages = t.body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        let content = messages[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "hello world");
        // No cache_control on a flat prompt
        assert!(content[0].get("cache_control").is_none());
        // No system field
        assert!(t.body.get("system").is_none());
        assert_eq!(t.body["model"], "claude-sonnet-4-5");
        assert_eq!(t.body["max_tokens"], 128);
    }

    #[test]
    fn translate_prompt_blocks_emits_cache_control() {
        let mut req = AiRequest::new("")
            .with_cached_prefix("PREFIX", "SUFFIX")
            .with_max_tokens(64);
        req.model = Some("claude-opus-4".into());
        let t = translate_request(&req, None).unwrap();

        let content = t.body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "PREFIX");
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert!(content[0]["cache_control"].get("ttl").is_none());
        assert_eq!(content[1]["text"], "SUFFIX");
        assert!(content[1].get("cache_control").is_none());
        assert!(!t.needs_extended_ttl_beta);
    }

    #[test]
    fn translate_extended_ttl_requires_beta() {
        let mut req = AiRequest::new("body").with_max_tokens(64);
        req.model = Some("claude-sonnet".into());
        req.prompt_blocks = Some(vec![
            PromptBlock::cached_with_ttl("big prefix", "1h"),
            PromptBlock::text("suffix"),
        ]);
        let t = translate_request(&req, None).unwrap();
        assert!(t.needs_extended_ttl_beta);
        let content = &t.body["messages"][0]["content"];
        assert_eq!(content[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(content[0]["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn translate_system_blocks() {
        let mut req = AiRequest::new("u").with_max_tokens(32);
        req.model = Some("m".into());
        req.system = Some(vec![
            PromptBlock::cached("static system"),
            PromptBlock::text("dynamic"),
        ]);
        let t = translate_request(&req, None).unwrap();
        let sys = t.body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2);
        assert_eq!(sys[0]["text"], "static system");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        assert!(sys[1].get("cache_control").is_none());
    }

    #[test]
    fn translate_conversation_history_with_cache_marker_on_last() {
        let mut req = AiRequest::new("final user").with_max_tokens(32);
        req.model = Some("m".into());
        req.conversation_history = vec![
            ConversationMessage::user("u1"),
            ConversationMessage::assistant("a1"),
        ];
        req.conversation_cache_control = CacheControl::Ephemeral;
        let t = translate_request(&req, None).unwrap();

        let messages = t.body["messages"].as_array().unwrap();
        // 2 history + 1 final user
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "u1");
        assert!(messages[0]["content"][0].get("cache_control").is_none());

        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["text"], "a1");
        // Last message of history is cache-marked
        assert_eq!(
            messages[1]["content"][0]["cache_control"]["type"],
            "ephemeral"
        );

        // Final user turn is not cache-marked (only history last was)
        assert_eq!(messages[2]["role"], "user");
        assert!(messages[2]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn translate_requires_max_tokens() {
        let mut req = AiRequest::new("x");
        req.model = Some("m".into());
        let err = translate_request(&req, None).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("max_tokens"),
            "expected max_tokens error, got: {msg}"
        );
    }

    #[test]
    fn translate_requires_model() {
        let req = AiRequest::new("x").with_max_tokens(32);
        let err = translate_request(&req, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("model"), "expected model error, got: {msg}");
    }

    #[test]
    fn translate_uses_adapter_default_model() {
        let req = AiRequest::new("x").with_max_tokens(32);
        let t = translate_request(&req, Some("claude-default")).unwrap();
        assert_eq!(t.body["model"], "claude-default");
    }

    #[test]
    fn translate_stop_sequences_passthrough() {
        let mut req = AiRequest::new("x").with_max_tokens(32);
        req.model = Some("m".into());
        req.stop_sequences = vec!["STOP".into(), "END".into()];
        let t = translate_request(&req, None).unwrap();
        let stops = t.body["stop_sequences"].as_array().unwrap();
        assert_eq!(stops.len(), 2);
        assert_eq!(stops[0], "STOP");
        assert_eq!(stops[1], "END");
    }

    // ----- Response deserialization -----

    #[test]
    fn parse_response_with_cache_usage() {
        let json = r#"{
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 3,
                "output_tokens": 7,
                "cache_creation_input_tokens": 1500,
                "cache_read_input_tokens": 0
            }
        }"#;
        let parsed: AnthropicMessagesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.usage.input_tokens, 3);
        assert_eq!(parsed.usage.output_tokens, 7);
        assert_eq!(parsed.usage.cache_creation_input_tokens, Some(1500));
        assert_eq!(parsed.usage.cache_read_input_tokens, Some(0));
        let block = &parsed.content[0];
        match block {
            AnthropicContentBlock::Text { text } => assert_eq!(text, "hello"),
            _ => panic!("expected text block"),
        }
    }

    #[test]
    fn parse_response_without_cache_fields() {
        let json = r#"{
            "content": [{"type": "text", "text": "hi"}],
            "usage": {"input_tokens": 1, "output_tokens": 2}
        }"#;
        let parsed: AnthropicMessagesResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.usage.cache_creation_input_tokens.is_none());
        assert!(parsed.usage.cache_read_input_tokens.is_none());
    }

    #[test]
    fn parse_response_multiple_text_blocks_concat() {
        let json = r#"{
            "content": [
                {"type": "text", "text": "part1 "},
                {"type": "text", "text": "part2"}
            ],
            "usage": {"input_tokens": 0, "output_tokens": 0}
        }"#;
        let parsed: AnthropicMessagesResponse = serde_json::from_str(json).unwrap();
        let mut text = String::new();
        for b in &parsed.content {
            if let AnthropicContentBlock::Text { text: t } = b {
                text.push_str(t);
            }
        }
        assert_eq!(text, "part1 part2");
    }

    #[test]
    fn parse_response_ignores_non_text_blocks() {
        let json = r#"{
            "content": [
                {"type": "text", "text": "visible"},
                {"type": "tool_use", "id": "abc", "name": "tool", "input": {}}
            ],
            "usage": {"input_tokens": 0, "output_tokens": 0}
        }"#;
        let parsed: AnthropicMessagesResponse = serde_json::from_str(json).unwrap();
        let mut text = String::new();
        for b in &parsed.content {
            if let AnthropicContentBlock::Text { text: t } = b {
                text.push_str(t);
            }
        }
        assert_eq!(text, "visible");
    }

    // ----- Error translation -----

    fn build_headers() -> reqwest::header::HeaderMap {
        reqwest::header::HeaderMap::new()
    }

    #[test]
    fn http_401_is_auth_error() {
        let body = br#"{"error":{"type":"authentication_error","message":"invalid key"}}"#;
        let err = http_error(reqwest::StatusCode::UNAUTHORIZED, &build_headers(), body);
        let msg = err.to_string();
        assert!(msg.contains("ANTHROPIC_API_KEY"));
        assert!(matches!(err, NodeError::AdapterError { .. }));
    }

    #[test]
    fn http_429_is_recoverable_with_retry_after() {
        let mut headers = build_headers();
        headers.insert("retry-after", "5".parse().unwrap());
        let body = br#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#;
        let err = http_error(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers, body);
        match err {
            NodeError::Failed {
                recoverable,
                message,
                ..
            } => {
                assert!(recoverable);
                assert!(message.contains("retry_after=5"));
                assert!(message.contains("rate_limit_error"));
            }
            other => panic!("expected recoverable Failed, got {other:?}"),
        }
    }

    #[test]
    fn http_529_is_recoverable() {
        let body = br#"{"error":{"type":"overloaded_error","message":"overloaded"}}"#;
        let err = http_error(
            reqwest::StatusCode::from_u16(529).unwrap(),
            &build_headers(),
            body,
        );
        match err {
            NodeError::Failed { recoverable, .. } => assert!(recoverable),
            other => panic!("expected recoverable Failed, got {other:?}"),
        }
    }

    #[test]
    fn http_500_is_recoverable() {
        let err = http_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            &build_headers(),
            b"{}",
        );
        match err {
            NodeError::Failed { recoverable, .. } => assert!(recoverable),
            other => panic!("expected recoverable Failed, got {other:?}"),
        }
    }

    #[test]
    fn http_400_surfaces_server_message() {
        let body = br#"{"error":{"type":"invalid_request_error","message":"bad shape"}}"#;
        let err = http_error(reqwest::StatusCode::BAD_REQUEST, &build_headers(), body);
        let msg = err.to_string();
        assert!(msg.contains("bad shape"));
        assert!(matches!(err, NodeError::AdapterError { .. }));
    }

    #[test]
    fn from_env_errors_when_unset() {
        // Snapshot-restore pattern to avoid leaking state across tests.
        let prior = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");
        let err = AnthropicApiAdapter::from_env().unwrap_err();
        assert!(err.to_string().contains("ANTHROPIC_API_KEY"));
        if let Some(v) = prior {
            std::env::set_var("ANTHROPIC_API_KEY", v);
        }
    }
}
