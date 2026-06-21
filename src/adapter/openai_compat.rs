//! Generic adapter for OpenAI-compatible Chat Completions endpoints.
//!
//! One adapter serves any provider that speaks the OpenAI `/v1/chat/completions`
//! wire format with a bearer token: OpenRouter, OpenAI, Groq, Together,
//! Fireworks, and local servers (vLLM, Ollama's OpenAI shim, …). The provider
//! is just a `(base_url, api_key, extra_headers)` triple — "OpenRouter" is this
//! adapter pointed at `https://openrouter.ai/api`.
//!
//! Arbitrary-model access comes for free: OpenRouter fronts hundreds of models
//! behind one endpoint, selected by the request `model` field
//! (`anthropic/claude-3.5-sonnet`, `openai/gpt-4o`, `meta-llama/...`). Set it
//! per-node via `config.model`.
//!
//! Stateless: there is no server-side session. Conversational continuity is
//! carried client-side via `AiRequest::conversation_history` (the same
//! ancestor-scoped history the engine assembles for every adapter).
//!
//! Non-streaming. Tool use, vision, and explicit prompt-cache markers are not
//! emitted — `cache_control` markers on prompt/system blocks are flattened to
//! plain text (OpenRouter applies provider-side caching automatically where
//! available).

use crate::adapter::{
    AdapterCapabilities, AiAdapter, AiRequest, AiResponse, MessageRole, TokenUsage,
};
use crate::error::NodeError;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api";
const OPENROUTER_NAME: &str = "openrouter";
const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
/// Fallback `max_tokens` for `judge`, which only needs a tiny integer reply.
const JUDGE_MAX_TOKENS: usize = 16;

/// Direct HTTP adapter for any OpenAI-compatible Chat Completions endpoint.
///
/// The `Debug` impl redacts the API key.
pub struct OpenAiCompatAdapter {
    /// Registry key and error label (e.g. "openrouter", "openai").
    name: String,
    api_key: String,
    default_model: Option<String>,
    base_url: String,
    client: Client,
    capabilities: AdapterCapabilities,
    /// Provider-recommended extra headers (e.g. OpenRouter `HTTP-Referer`,
    /// `X-Title`). Sent verbatim on every request.
    extra_headers: Vec<(String, String)>,
}

impl OpenAiCompatAdapter {
    /// Construct for an arbitrary OpenAI-compatible endpoint.
    ///
    /// `base_url` is the host root with no trailing `/v1` (e.g.
    /// `https://openrouter.ai/api`, `https://api.openai.com`); the adapter
    /// appends `/v1/chat/completions`.
    pub fn new(
        name: impl Into<String>,
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            api_key: api_key.into(),
            default_model: None,
            base_url: base_url.into(),
            client: Client::new(),
            capabilities: AdapterCapabilities {
                tool_use: false,
                // Supported via `response_format`, but model-dependent on
                // OpenRouter — callers needing a guarantee should pick a model
                // that supports structured outputs.
                structured_output: true,
                vision: false,
                conversation_history: true,
                // Varies per routed model; left unset so capability validation
                // never over-promises a token ceiling.
                max_tokens: None,
            },
            extra_headers: Vec::new(),
        }
    }

    /// Construct the OpenRouter preset from `OPENROUTER_API_KEY`.
    ///
    /// Errors when the env var is unset or empty — a hard configuration error,
    /// not something to recover from at runtime.
    pub fn openrouter_from_env() -> Result<Self, NodeError> {
        let key = std::env::var(OPENROUTER_API_KEY_ENV)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| NodeError::AdapterError {
                adapter: OPENROUTER_NAME.into(),
                message: format!("{OPENROUTER_API_KEY_ENV} is not set"),
            })?;
        Ok(Self::new(OPENROUTER_NAME, key, OPENROUTER_BASE_URL))
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Override the base URL. Include scheme + host, no trailing slash and no
    /// `/v1` suffix.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override the registry name / error label.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_capabilities(mut self, caps: AdapterCapabilities) -> Self {
        self.capabilities = caps;
        self
    }

    /// Add a provider-recommended header (e.g. OpenRouter's `HTTP-Referer` /
    /// `X-Title` for attribution). Repeatable.
    pub fn with_header(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((key.into(), value.into()));
        self
    }

    fn redacted_key(&self) -> String {
        if self.api_key.len() <= 4 {
            "<redacted>".into()
        } else {
            format!("<redacted:len={}>", self.api_key.len())
        }
    }
}

impl std::fmt::Debug for OpenAiCompatAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatAdapter")
            .field("name", &self.name)
            .field("api_key", &self.redacted_key())
            .field("default_model", &self.default_model)
            .field("base_url", &self.base_url)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize, Debug)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<serde_json::Value>,
}

#[derive(Serialize, Debug)]
struct ChatMessage {
    role: &'static str,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: ChatUsage,
}

#[derive(Deserialize, Default)]
struct Choice {
    #[serde(default)]
    message: ChoiceMessage,
}

#[derive(Deserialize, Default)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Deserialize, Default)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: usize,
    #[serde(default)]
    completion_tokens: usize,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Deserialize, Default)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<usize>,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<ErrorDetail>,
}

#[derive(Deserialize)]
struct ErrorDetail {
    message: Option<String>,
    #[serde(rename = "type")]
    ty: Option<String>,
}

// ---------------------------------------------------------------------------
// Request translation
// ---------------------------------------------------------------------------

/// Flatten prompt/system blocks to plain text. `cache_control` markers carry
/// no equivalent in the OpenAI wire format, so they are dropped here.
fn blocks_to_text(blocks: &[crate::adapter::PromptBlock]) -> String {
    blocks
        .iter()
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

fn translate_request(
    req: &AiRequest,
    default_model: Option<&str>,
    name: &str,
) -> Result<ChatRequest, NodeError> {
    let model = req
        .model
        .as_deref()
        .or(default_model)
        .ok_or_else(|| NodeError::AdapterError {
            adapter: name.into(),
            message: "no model specified on request or adapter".into(),
        })?
        .to_string();

    let mut messages: Vec<ChatMessage> = Vec::new();

    // System blocks collapse into a single leading system message.
    if let Some(system) = &req.system {
        let text = blocks_to_text(system);
        if !text.is_empty() {
            messages.push(ChatMessage {
                role: "system",
                content: text,
            });
        }
    }

    // Prior turns.
    for msg in &req.conversation_history {
        let role = match msg.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => "system",
        };
        messages.push(ChatMessage {
            role,
            content: msg.content.clone(),
        });
    }

    // Final user turn: prefer structured blocks (flattened), else flat prompt.
    let user_content = match &req.prompt_blocks {
        Some(blocks) => blocks_to_text(blocks),
        None => req.prompt.clone(),
    };
    messages.push(ChatMessage {
        role: "user",
        content: user_content,
    });

    // Structured output via OpenAI `response_format`. `strict` is omitted
    // (defaults false) for broad cross-provider acceptance on OpenRouter.
    let response_format = req.output_schema.as_ref().map(|schema| {
        serde_json::json!({
            "type": "json_schema",
            "json_schema": { "name": "response", "schema": schema }
        })
    });

    Ok(ChatRequest {
        model,
        messages,
        temperature: req.temperature,
        max_tokens: req.max_tokens,
        stop: req.stop_sequences.clone(),
        response_format,
    })
}

// ---------------------------------------------------------------------------
// Adapter impl
// ---------------------------------------------------------------------------

impl AiAdapter for OpenAiCompatAdapter {
    fn complete(
        &self,
        req: AiRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AiResponse, NodeError>> + Send + '_>> {
        Box::pin(async move {
            let start = Instant::now();

            let body = translate_request(&req, self.default_model.as_deref(), &self.name)?;

            let url = format!(
                "{}/v1/chat/completions",
                self.base_url.trim_end_matches('/')
            );
            let mut builder = self
                .client
                .post(&url)
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json");
            for (k, v) in &self.extra_headers {
                builder = builder.header(k.as_str(), v.as_str());
            }

            let resp = builder
                .json(&body)
                .send()
                .await
                .map_err(|e| network_error(e, &self.name))?;

            let status = resp.status();
            let headers = resp.headers().clone();
            let body_bytes = resp.bytes().await.map_err(|e| NodeError::AdapterError {
                adapter: self.name.clone(),
                message: format!("failed to read response body: {e}"),
            })?;

            let latency_ms = start.elapsed().as_millis() as u64;

            if !status.is_success() {
                return Err(http_error(status, &headers, &body_bytes, &self.name));
            }

            let parsed: ChatResponse =
                serde_json::from_slice(&body_bytes).map_err(|e| NodeError::AdapterError {
                    adapter: self.name.clone(),
                    message: format!("failed to parse response: {e}"),
                })?;

            let text = parsed
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .unwrap_or_default();

            let cache_read = parsed
                .usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens);

            let usage = TokenUsage {
                input_tokens: parsed.usage.prompt_tokens,
                output_tokens: parsed.usage.completion_tokens,
                cache_read_input_tokens: cache_read,
                cache_creation_input_tokens: None,
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
            req.max_tokens = Some(JUDGE_MAX_TOKENS);
        }
        let num = candidates.len();
        let name = self.name.clone();

        Box::pin(async move {
            let resp = self.complete(req).await?;
            let text = resp.text.trim();
            let idx = text.parse::<usize>().map_err(|_| NodeError::AdapterError {
                adapter: name.clone(),
                message: format!("judge response was not a valid index: '{text}'"),
            })?;
            if idx >= num {
                return Err(NodeError::AdapterError {
                    adapter: name,
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
        &self.name
    }
}

// ---------------------------------------------------------------------------
// Error translation
// ---------------------------------------------------------------------------

fn network_error(err: reqwest::Error, name: &str) -> NodeError {
    let recoverable = err.is_timeout() || err.is_connect();
    if recoverable {
        NodeError::Failed {
            source_message: None,
            message: format!("{name} network error: {err}"),
            recoverable: true,
        }
    } else {
        NodeError::AdapterError {
            adapter: name.into(),
            message: format!("network error: {err}"),
        }
    }
}

fn http_error(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: &[u8],
    name: &str,
) -> NodeError {
    let server_message = serde_json::from_slice::<ErrorBody>(body)
        .ok()
        .and_then(|b| b.error)
        .and_then(|e| {
            let ty = e.ty.unwrap_or_default();
            let msg = e.message.unwrap_or_default();
            if ty.is_empty() && msg.is_empty() {
                None
            } else if ty.is_empty() {
                Some(msg)
            } else {
                Some(format!("{ty}: {msg}"))
            }
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());

    match status.as_u16() {
        401 | 403 => NodeError::AdapterError {
            adapter: name.into(),
            message: format!("auth error ({status}); check API key: {server_message}"),
        },
        404 => NodeError::AdapterError {
            adapter: name.into(),
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
                    "{name} rate_limit ({status}), retry_after={retry_after}: {server_message}"
                ),
                recoverable: true,
            }
        }
        code if (500..600).contains(&code) => NodeError::Failed {
            source_message: None,
            message: format!("{name} server error ({status}): {server_message}"),
            recoverable: true,
        },
        _ => NodeError::AdapterError {
            adapter: name.into(),
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
    use crate::adapter::PromptBlock;

    fn sample_request() -> AiRequest {
        let mut r = AiRequest::new("hello world").with_max_tokens(128);
        r.model = Some("openai/gpt-4o".into());
        r
    }

    #[test]
    fn openrouter_preset_name_and_base() {
        let a = OpenAiCompatAdapter::new(OPENROUTER_NAME, "sk-test", OPENROUTER_BASE_URL);
        assert_eq!(a.name(), "openrouter");
        assert_eq!(a.base_url, "https://openrouter.ai/api");
    }

    #[test]
    fn debug_redacts_api_key() {
        let a = OpenAiCompatAdapter::new("openrouter", "sk-or-supersecret", OPENROUTER_BASE_URL);
        let s = format!("{a:?}");
        assert!(!s.contains("supersecret"));
        assert!(s.contains("redacted"));
    }

    #[test]
    fn capabilities_defaults() {
        let a = OpenAiCompatAdapter::new("openrouter", "k", OPENROUTER_BASE_URL);
        assert!(!a.capabilities().tool_use);
        assert!(a.capabilities().structured_output);
        assert!(a.capabilities().conversation_history);
        assert_eq!(a.capabilities().max_tokens, None);
    }

    #[test]
    fn translate_flat_prompt() {
        let req = sample_request();
        let body = translate_request(&req, None, "openrouter").unwrap();
        assert_eq!(body.model, "openai/gpt-4o");
        assert_eq!(body.max_tokens, Some(128));
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].role, "user");
        assert_eq!(body.messages[0].content, "hello world");
        assert!(body.response_format.is_none());
    }

    #[test]
    fn translate_system_and_history_ordered() {
        let mut req = AiRequest::new("final user").with_max_tokens(32);
        req.model = Some("m".into());
        req.system = Some(vec![PromptBlock::text("you are terse")]);
        req.conversation_history = vec![
            ConversationMessage::user("u1"),
            ConversationMessage::assistant("a1"),
        ];
        let body = translate_request(&req, None, "openrouter").unwrap();
        // system, u1, a1, final user
        assert_eq!(body.messages.len(), 4);
        assert_eq!(body.messages[0].role, "system");
        assert_eq!(body.messages[0].content, "you are terse");
        assert_eq!(body.messages[1].role, "user");
        assert_eq!(body.messages[1].content, "u1");
        assert_eq!(body.messages[2].role, "assistant");
        assert_eq!(body.messages[3].role, "user");
        assert_eq!(body.messages[3].content, "final user");
    }

    #[test]
    fn translate_prompt_blocks_flattened() {
        let req = AiRequest::new("")
            .with_cached_prefix("PREFIX", "SUFFIX")
            .with_max_tokens(64);
        let mut req = req;
        req.model = Some("m".into());
        let body = translate_request(&req, None, "openrouter").unwrap();
        // Blocks collapse to one user message; cache markers dropped.
        assert_eq!(body.messages.len(), 1);
        assert_eq!(body.messages[0].content, "PREFIXSUFFIX");
    }

    #[test]
    fn translate_output_schema_emits_response_format() {
        let mut req = sample_request();
        req.output_schema = Some(serde_json::json!({
            "type": "object",
            "properties": { "role": { "enum": ["claim", "question"] } }
        }));
        let body = translate_request(&req, None, "openrouter").unwrap();
        let rf = body.response_format.expect("response_format set");
        assert_eq!(rf["type"], "json_schema");
        assert_eq!(rf["json_schema"]["name"], "response");
        assert_eq!(
            rf["json_schema"]["schema"]["properties"]["role"]["enum"][0],
            "claim"
        );
    }

    #[test]
    fn translate_uses_adapter_default_model() {
        let req = AiRequest::new("x").with_max_tokens(32);
        let body = translate_request(&req, Some("openrouter/auto"), "openrouter").unwrap();
        assert_eq!(body.model, "openrouter/auto");
    }

    #[test]
    fn translate_requires_model() {
        let req = AiRequest::new("x").with_max_tokens(32);
        let err = translate_request(&req, None, "openrouter").unwrap_err();
        assert!(err.to_string().contains("model"));
    }

    #[test]
    fn translate_omits_max_tokens_when_absent() {
        // Unlike Anthropic, OpenAI-compatible endpoints treat max_tokens as
        // optional — it must not be forced.
        let mut req = AiRequest::new("x");
        req.model = Some("m".into());
        let body = translate_request(&req, None, "openrouter").unwrap();
        assert_eq!(body.max_tokens, None);
    }

    #[test]
    fn translate_stop_sequences_passthrough() {
        let mut req = sample_request();
        req.stop_sequences = vec!["STOP".into()];
        let body = translate_request(&req, None, "openrouter").unwrap();
        assert_eq!(body.stop, vec!["STOP".to_string()]);
    }

    // ----- Response deserialization -----

    #[test]
    fn parse_response_with_content_and_usage() {
        let json = r#"{
            "choices": [{"message": {"role": "assistant", "content": "hello"}}],
            "usage": {"prompt_tokens": 3, "completion_tokens": 7,
                      "prompt_tokens_details": {"cached_tokens": 2}}
        }"#;
        let parsed: ChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.choices[0].message.content.as_deref(), Some("hello"));
        assert_eq!(parsed.usage.prompt_tokens, 3);
        assert_eq!(parsed.usage.completion_tokens, 7);
        assert_eq!(
            parsed.usage.prompt_tokens_details.unwrap().cached_tokens,
            Some(2)
        );
    }

    #[test]
    fn parse_response_without_cache_details() {
        let json = r#"{
            "choices": [{"message": {"content": "hi"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2}
        }"#;
        let parsed: ChatResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.usage.prompt_tokens_details.is_none());
    }

    #[test]
    fn parse_response_null_content_is_empty() {
        let json = r#"{"choices": [{"message": {"content": null}}], "usage": {}}"#;
        let parsed: ChatResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.choices[0].message.content.is_none());
    }

    // ----- Error translation -----

    fn headers() -> reqwest::header::HeaderMap {
        reqwest::header::HeaderMap::new()
    }

    #[test]
    fn http_401_is_auth_error() {
        let body = br#"{"error":{"type":"authentication_error","message":"bad key"}}"#;
        let err = http_error(
            reqwest::StatusCode::UNAUTHORIZED,
            &headers(),
            body,
            "openrouter",
        );
        assert!(matches!(err, NodeError::AdapterError { .. }));
        assert!(err.to_string().contains("API key"));
    }

    #[test]
    fn http_429_is_recoverable() {
        let mut h = headers();
        h.insert("retry-after", "5".parse().unwrap());
        let body = br#"{"error":{"message":"slow down"}}"#;
        let err = http_error(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            &h,
            body,
            "openrouter",
        );
        match err {
            NodeError::Failed {
                recoverable,
                message,
                ..
            } => {
                assert!(recoverable);
                assert!(message.contains("retry_after=5"));
            }
            other => panic!("expected recoverable Failed, got {other:?}"),
        }
    }

    #[test]
    fn http_500_is_recoverable() {
        let err = http_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            &headers(),
            b"{}",
            "openrouter",
        );
        assert!(matches!(
            err,
            NodeError::Failed {
                recoverable: true,
                ..
            }
        ));
    }

    #[test]
    fn http_400_surfaces_server_message() {
        let body = br#"{"error":{"type":"invalid_request_error","message":"bad shape"}}"#;
        let err = http_error(
            reqwest::StatusCode::BAD_REQUEST,
            &headers(),
            body,
            "openrouter",
        );
        assert!(err.to_string().contains("bad shape"));
        assert!(matches!(err, NodeError::AdapterError { .. }));
    }

    #[test]
    fn openrouter_from_env_errors_when_unset() {
        let prior = std::env::var(OPENROUTER_API_KEY_ENV).ok();
        std::env::remove_var(OPENROUTER_API_KEY_ENV);
        let err = OpenAiCompatAdapter::openrouter_from_env().unwrap_err();
        assert!(err.to_string().contains(OPENROUTER_API_KEY_ENV));
        if let Some(v) = prior {
            std::env::set_var(OPENROUTER_API_KEY_ENV, v);
        }
    }
}
