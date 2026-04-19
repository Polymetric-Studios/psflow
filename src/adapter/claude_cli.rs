use crate::adapter::{AdapterCapabilities, AiAdapter, AiRequest, AiResponse, TokenUsage};
use crate::error::NodeError;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Instant;
use tokio::process::Command;

/// AI adapter that delegates to the Claude Code CLI (`claude`) subprocess.
///
/// Each `complete` call spawns `claude -p <prompt> --output-format json`,
/// captures stdout, and parses the response. Conversation context is
/// maintained across calls using `--resume <session_id>`.
///
/// This is the primary development adapter for Ergon workflows.
pub struct ClaudeCliAdapter {
    /// Path to the `claude` binary. Defaults to `"claude"` (on PATH).
    command: String,
    /// Session ID for conversation continuity via `--resume`.
    session_id: Mutex<Option<String>>,
    /// Model override (e.g., "claude-sonnet-4-20250514").
    default_model: Option<String>,
    capabilities: AdapterCapabilities,
}

impl ClaudeCliAdapter {
    pub fn new() -> Self {
        Self {
            command: "claude".into(),
            session_id: Mutex::new(None),
            default_model: None,
            capabilities: AdapterCapabilities {
                tool_use: true,
                structured_output: true,
                vision: false,
                conversation_history: true,
                max_tokens: Some(200_000),
            },
        }
    }

    /// Set the path to the `claude` binary.
    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.command = command.into();
        self
    }

    /// Set the default model for all requests.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// Set an existing session ID to resume a conversation.
    pub fn with_session(self, session_id: impl Into<String>) -> Self {
        *self.session_id.lock().unwrap() = Some(session_id.into());
        self
    }

    fn build_command(&self, req: &AiRequest) -> Command {
        let mut cmd = Command::new(&self.command);

        // Print mode: single prompt, output to stdout
        cmd.arg("-p").arg(&req.prompt);

        // JSON output for structured parsing
        cmd.arg("--output-format").arg("json");

        // Model selection: request-level overrides adapter default
        if let Some(ref model) = req.model {
            cmd.arg("--model").arg(model);
        } else if let Some(ref model) = self.default_model {
            cmd.arg("--model").arg(model);
        }

        // Max tokens
        if let Some(tokens) = req.max_tokens {
            cmd.arg("--max-tokens").arg(tokens.to_string());
        }

        // Resume conversation if we have a session ID
        let guard = self.session_id.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(ref id) = *guard {
            cmd.arg("--resume").arg(id);
        }
        drop(guard);

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        cmd
    }
}

impl Default for ClaudeCliAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AiAdapter for ClaudeCliAdapter {
    fn complete(
        &self,
        req: AiRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AiResponse, NodeError>> + Send + '_>> {
        let mut cmd = self.build_command(&req);
        let start = Instant::now();

        Box::pin(async move {
            let output = cmd.output().await.map_err(|e| NodeError::AdapterError {
                adapter: "claude_cli".into(),
                message: format!("failed to spawn claude process: {e}"),
            })?;

            let latency_ms = start.elapsed().as_millis() as u64;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(NodeError::AdapterError {
                    adapter: "claude_cli".into(),
                    message: format!("claude exited with {}: {}", output.status, stderr.trim()),
                });
            }

            let stdout = String::from_utf8_lossy(&output.stdout);

            // Parse JSON output from claude CLI
            // The --output-format json produces: {"type":"result","result":"...","session_id":"..."}
            let parsed: serde_json::Value =
                serde_json::from_str(&stdout).map_err(|e| NodeError::AdapterError {
                    adapter: "claude_cli".into(),
                    message: format!("failed to parse claude JSON output: {e}"),
                })?;

            // Extract the response text
            let text = parsed
                .get("result")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| stdout.trim())
                .to_string();

            // Store session ID for conversation continuity
            if let Some(sid) = parsed.get("session_id").and_then(|v| v.as_str()) {
                let mut guard = self.session_id.lock().unwrap_or_else(|e| e.into_inner());
                *guard = Some(sid.to_string());
            }

            // Extract usage if available
            let usage = if let Some(usage_obj) = parsed.get("usage") {
                TokenUsage {
                    input_tokens: usage_obj
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize,
                    output_tokens: usage_obj
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0) as usize,
                    ..Default::default()
                }
            } else {
                TokenUsage::default()
            };

            // Try to parse structured output if requested
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

        // Build a judge prompt
        let mut prompt =
            format!("Given these candidates, select the best one based on: {criteria}\n\n");
        for (i, candidate) in candidates.iter().enumerate() {
            prompt.push_str(&format!("Candidate {i}: {candidate}\n\n"));
        }
        prompt.push_str("Respond with ONLY the candidate number (0-based index). Nothing else.");

        let req = AiRequest::new(prompt).with_temperature(0.0);
        let num_candidates = candidates.len();

        Box::pin(async move {
            let response = self.complete(req).await?;
            let text = response.text.trim();

            let idx = text.parse::<usize>().map_err(|_| NodeError::AdapterError {
                adapter: "claude_cli".into(),
                message: format!("judge response was not a valid index: '{text}'"),
            })?;

            if idx >= num_candidates {
                return Err(NodeError::AdapterError {
                    adapter: "claude_cli".into(),
                    message: format!(
                        "judge returned index {idx} but only {num_candidates} candidates exist"
                    ),
                });
            }

            Ok(idx)
        })
    }

    fn capabilities(&self) -> &AdapterCapabilities {
        &self.capabilities
    }

    fn name(&self) -> &str {
        "claude_cli"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capabilities() {
        let adapter = ClaudeCliAdapter::new();
        assert!(adapter.capabilities().tool_use);
        assert!(adapter.capabilities().structured_output);
        assert!(adapter.capabilities().conversation_history);
        assert!(!adapter.capabilities().vision);
    }

    #[test]
    fn name_is_claude_cli() {
        assert_eq!(ClaudeCliAdapter::new().name(), "claude_cli");
    }

    #[test]
    fn builder_methods() {
        let adapter = ClaudeCliAdapter::new()
            .with_command("/usr/local/bin/claude")
            .with_model("claude-sonnet-4-20250514")
            .with_session("sess_123");

        assert_eq!(adapter.command, "/usr/local/bin/claude");
        assert_eq!(
            adapter.default_model,
            Some("claude-sonnet-4-20250514".into())
        );
        assert_eq!(*adapter.session_id.lock().unwrap(), Some("sess_123".into()));
    }

    #[test]
    fn build_command_basic() {
        let adapter = ClaudeCliAdapter::new();
        let req = AiRequest::new("Hello");
        let cmd = adapter.build_command(&req);
        let prog = cmd.as_std().get_program().to_str().unwrap();
        assert_eq!(prog, "claude");

        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();
        assert!(args.contains(&"-p"));
        assert!(args.contains(&"Hello"));
        assert!(args.contains(&"--output-format"));
        assert!(args.contains(&"json"));
    }

    #[test]
    fn build_command_with_model_and_tokens() {
        let adapter = ClaudeCliAdapter::new();
        let req = AiRequest::new("test")
            .with_model("claude-haiku")
            .with_max_tokens(500);

        let cmd = adapter.build_command(&req);
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();

        assert!(args.contains(&"--model"));
        assert!(args.contains(&"claude-haiku"));
        assert!(args.contains(&"--max-tokens"));
        assert!(args.contains(&"500"));
    }

    #[test]
    fn build_command_with_resume() {
        let adapter = ClaudeCliAdapter::new().with_session("sess_abc");
        let req = AiRequest::new("continue");

        let cmd = adapter.build_command(&req);
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();

        assert!(args.contains(&"--resume"));
        assert!(args.contains(&"sess_abc"));
    }

    #[test]
    fn adapter_default_model_used_when_request_has_none() {
        let adapter = ClaudeCliAdapter::new().with_model("claude-opus");
        let req = AiRequest::new("test"); // no model override

        let cmd = adapter.build_command(&req);
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();

        assert!(args.contains(&"--model"));
        assert!(args.contains(&"claude-opus"));
    }

    #[test]
    fn request_model_overrides_default() {
        let adapter = ClaudeCliAdapter::new().with_model("claude-opus");
        let req = AiRequest::new("test").with_model("claude-haiku");

        let cmd = adapter.build_command(&req);
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect();

        // Request model should win
        assert!(args.contains(&"claude-haiku"));
        // Adapter default should NOT appear
        assert!(!args.contains(&"claude-opus"));
    }
}
