//! `claude_workflow` handler — run a prompt (often a Claude Code dynamic
//! workflow) as a psflow node by driving the real interactive `claude` TUI
//! headless over a pseudo-terminal.
//!
//! psflow owns the durable/triggered/scheduled outside; a Claude Code session
//! (and any agent swarm a workflow spawns) runs inside one node. The node
//! returns the turn's final assistant message — read deterministically from the
//! session transcript — as a typed output.
//!
//! Requires the `terminal` feature. The blocking [`ClaudeTerminalSession`] runs
//! on a [`tokio::task::spawn_blocking`] thread; the node's cancellation token is
//! mapped onto the session's cancel flag.

use crate::adapter::{
    ApprovalChoice, ApprovalPolicy, ClaudeTerminalSession, ResultSource, SessionOptions,
    TerminalError,
};
use crate::error::NodeError;
use crate::execute::blackboard::Blackboard;
use crate::execute::{CancellationToken, HandlerSchema, NodeHandler, Outputs, SchemaField};
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::template::TemplateResolver;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_OUTPUT_KEY: &str = "result";
/// Non-prompting default so a headless run is autonomous; the `AllowAll` policy
/// is the backstop for any dialog that still appears.
const DEFAULT_PERMISSION_MODE: &str = "acceptEdits";

/// Runs a prompt in a real interactive `claude` session via a PTY.
///
/// ## Configuration
///
/// - `config.prompt` (required): the task/prompt. Template-resolved against node
///   inputs. Include the word `workflow` to have Claude orchestrate a dynamic
///   workflow.
/// - `config.output`: output key for the result text (default `result`).
/// - `config.model`: model override (`--model`).
/// - `config.permission_mode`: `--permission-mode` (default `acceptEdits`).
/// - `config.approval`: `allow` (default) or `deny` — how dialogs are answered
///   if any appear despite the permission mode.
/// - `config.timeout_ms`: per-turn timeout (default from `SessionOptions`).
/// - `config.cwd`: working directory for the session.
/// - `config.args`: extra `claude` CLI flags (e.g. `--mcp-config`, a path).
///
/// ## Outputs
///
/// - `<output>`: the final assistant message of the turn (String).
/// - `source`: `transcript` (deterministic) or `screen` (scrape fallback).
pub struct ClaudeWorkflowHandler {
    resolver: Arc<dyn TemplateResolver>,
}

impl ClaudeWorkflowHandler {
    pub fn new(resolver: Arc<dyn TemplateResolver>) -> Self {
        Self { resolver }
    }
}

/// Parsed, owned config for one run (extracted before the async move).
struct RunConfig {
    prompt: String,
    output_key: String,
    opts: SessionOptions,
    policy: ApprovalPolicy,
    /// When true, route dialogs to a notifier (and defer the decision to the
    /// human via the session's remote-control URL).
    notify: bool,
}

impl NodeHandler for ClaudeWorkflowHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let config = node.config.clone();
        let node_id = node.id.0.clone();
        let resolver = self.resolver.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before claude_workflow".into(),
                });
            }

            let fail = |msg: String| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': {msg}"),
                recoverable: false,
            };

            // --- Parse config ---
            let prompt_tpl = config
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| fail("missing config.prompt".into()))?;
            let bb = Blackboard::new();
            let prompt = resolver
                .render(prompt_tpl, &inputs, &bb)
                .map_err(|e| fail(format!("prompt template error: {e}")))?;

            let output_key = config
                .get("output")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_OUTPUT_KEY)
                .to_string();

            let permission_mode = config
                .get("permission_mode")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_PERMISSION_MODE);

            // Cancel flag shared with the blocking session (and the `ask` policy).
            let cancel_flag = Arc::new(AtomicBool::new(false));

            // allow (auto-yes) | deny (auto-no) | notify (route to a human via the
            // session's remote-control URL) | ask (route into this process via a
            // file channel — a request file appears and we block on a response file,
            // so the surrounding session/operator can answer in-place).
            let approval = config.get("approval").and_then(|v| v.as_str());
            let wait_ms = config
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .map(|t| t as u128)
                .unwrap_or(300_000);
            let (policy, notify) = match approval {
                Some("deny") => (ApprovalPolicy::DenyAll, false),
                Some("notify") => (ApprovalPolicy::custom(|_| ApprovalChoice::Defer), true),
                Some("ask") => {
                    let dir = std::env::temp_dir().join("psflow-approvals");
                    let _ = std::fs::create_dir_all(&dir);
                    let req = dir.join(format!("{node_id}.request.json"));
                    let resp = dir.join(format!("{node_id}.response"));
                    let _ = std::fs::remove_file(&req);
                    let _ = std::fs::remove_file(&resp);
                    tracing::warn!(
                        "[claude_workflow] node '{node_id}' approval channel ready — a request will appear at {}; write your choice (allow|deny|<n>) to {}",
                        req.display(),
                        resp.display()
                    );
                    (
                        file_channel_policy(req, resp, wait_ms, cancel_flag.clone()),
                        false,
                    )
                }
                _ => (ApprovalPolicy::AllowAll, false),
            };

            let mut opts = SessionOptions::default()
                .with_arg("--permission-mode")
                .with_arg(permission_mode);
            if let Some(model) = config.get("model").and_then(|v| v.as_str()) {
                opts = opts.with_model(model);
            }
            if let Some(cwd) = config.get("cwd").and_then(|v| v.as_str()) {
                opts = opts.with_cwd(cwd);
            }
            if let Some(args) = config.get("args").and_then(|v| v.as_array()) {
                for a in args.iter().filter_map(|v| v.as_str()) {
                    opts = opts.with_arg(a);
                }
            }
            if let Some(t) = config.get("timeout_ms").and_then(|v| v.as_u64()) {
                opts.turn_timeout_ms = t as u128;
            }

            let run = RunConfig {
                prompt,
                output_key,
                opts,
                policy,
                notify,
            };

            // --- Drive the session on a blocking thread, mapping cancellation ---
            let cf = cancel_flag.clone();
            let notify_node_id = node_id.clone();
            let mut handle = tokio::task::spawn_blocking(move || -> Result<_, TerminalError> {
                let mut session = ClaudeTerminalSession::spawn_ready(run.opts)?;
                session.set_cancel_flag(cf);
                session.set_approval_policy(run.policy);
                if run.notify {
                    let nid = notify_node_id.clone();
                    session.set_approval_notifier(Arc::new(move |prompt, url| {
                        tracing::warn!(
                            "[claude_workflow] node '{nid}' needs approval: {} | options={:?} | answer at: {}",
                            prompt.question,
                            prompt.options,
                            url.unwrap_or("(no remote-control URL on screen)")
                        );
                    }));
                }
                let turn = session.run_turn(&run.prompt)?;
                Ok((turn, session.session_id().to_string()))
            });

            let (turn, session_id) = tokio::select! {
                joined = &mut handle => match joined {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => return Err(map_terminal_error(&node_id, e)),
                    Err(join) => return Err(fail(format!("session task panicked: {join}"))),
                },
                _ = cancel.cancelled() => {
                    cancel_flag.store(true, Ordering::SeqCst);
                    let _ = handle.await; // let the blocking task observe the flag and tear down
                    return Err(NodeError::Cancelled {
                        reason: "cancelled during claude_workflow".into(),
                    });
                }
            };

            let source = match turn.source {
                ResultSource::Transcript => "transcript",
                ResultSource::Screen => "screen",
            };
            let mut outputs = Outputs::new();
            outputs.insert(run.output_key.clone(), Value::String(turn.result));
            outputs.insert("source".into(), Value::String(source.into()));
            outputs.insert("session_id".into(), Value::String(session_id));
            Ok(outputs)
        })
    }

    fn schema(&self, name: &str) -> HandlerSchema {
        HandlerSchema::new(name, "Run a prompt/workflow in a real claude TUI session over a PTY")
            .with_config(
                SchemaField::new("prompt", "string")
                    .required()
                    .describe("Task/prompt, template-resolved. Include 'workflow' to orchestrate one."),
            )
            .with_config(
                SchemaField::new("output", "string")
                    .describe("Output key for the result text")
                    .default(serde_json::json!(DEFAULT_OUTPUT_KEY)),
            )
            .with_config(SchemaField::new("model", "string").describe("Model override (--model)"))
            .with_config(
                SchemaField::new("permission_mode", "string")
                    .describe("claude --permission-mode")
                    .default(serde_json::json!(DEFAULT_PERMISSION_MODE)),
            )
            .with_config(
                SchemaField::new("approval", "string")
                    .describe("allow | deny | notify (remote-control) | ask (file channel: write choice to a response file)")
                    .default(serde_json::json!("allow")),
            )
            .with_config(SchemaField::new("timeout_ms", "integer").describe("Per-turn timeout"))
            .with_config(SchemaField::new("cwd", "string").describe("Working directory"))
            .with_config(SchemaField::new("args", "array<string>").describe("Extra claude CLI flags"))
            .with_output(SchemaField::new(DEFAULT_OUTPUT_KEY, "string"))
            .with_output(SchemaField::new("source", "string"))
            .with_output(SchemaField::new("session_id", "string"))
    }
}

/// Parse a written approval response into a choice. `allow`/`yes`/`y`/`1` →
/// Allow, `deny`/`no`/`n` → Deny, a bare number → that option, else Deny.
fn parse_choice(s: &str) -> ApprovalChoice {
    match s.trim().to_lowercase().as_str() {
        "allow" | "yes" | "y" | "1" => ApprovalChoice::Allow,
        "deny" | "no" | "n" => ApprovalChoice::Deny,
        other => other
            .parse::<usize>()
            .map(ApprovalChoice::Select)
            .unwrap_or(ApprovalChoice::Deny),
    }
}

/// An `ApprovalPolicy` that routes each dialog through a pair of files: it writes
/// the prompt to `req` and blocks polling `resp` for a written choice (so the
/// surrounding process/operator answers in-place). Returns `Deny` on cancel or
/// after `deadline_ms` with no response.
fn file_channel_policy(
    req: PathBuf,
    resp: PathBuf,
    deadline_ms: u128,
    cancel: Arc<AtomicBool>,
) -> ApprovalPolicy {
    ApprovalPolicy::custom(move |prompt| {
        let payload = serde_json::json!({
            "question": prompt.question,
            "options": prompt.options,
            "respond_to": resp.to_string_lossy(),
        });
        let _ = std::fs::write(
            &req,
            serde_json::to_vec_pretty(&payload).unwrap_or_default(),
        );
        let _ = std::fs::remove_file(&resp);

        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(300));
            if cancel.load(Ordering::Relaxed) {
                let _ = std::fs::remove_file(&req);
                return ApprovalChoice::Deny;
            }
            if let Ok(s) = std::fs::read_to_string(&resp) {
                if !s.trim().is_empty() {
                    let _ = std::fs::remove_file(&resp);
                    let _ = std::fs::remove_file(&req);
                    return parse_choice(&s);
                }
            }
            if start.elapsed().as_millis() >= deadline_ms {
                let _ = std::fs::remove_file(&req);
                return ApprovalChoice::Deny;
            }
        }
    })
}

fn map_terminal_error(node_id: &str, e: TerminalError) -> NodeError {
    match e {
        TerminalError::Cancelled => NodeError::Cancelled {
            reason: format!("node '{node_id}': claude session cancelled"),
        },
        TerminalError::Timeout(ms, what) => NodeError::Failed {
            source_message: None,
            message: format!(
                "node '{node_id}': claude session timed out after {ms}ms waiting for {what}"
            ),
            recoverable: true,
        },
        other => NodeError::Failed {
            source_message: Some(other.to_string()),
            message: format!("node '{node_id}': claude session error: {other}"),
            recoverable: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::PromptTemplateResolver;

    fn handler() -> ClaudeWorkflowHandler {
        ClaudeWorkflowHandler::new(Arc::new(PromptTemplateResolver))
    }

    #[tokio::test]
    async fn missing_prompt_errors_before_spawn() {
        let node = Node::new("cw", "Claude").with_handler("claude_workflow");
        let result = handler()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing config.prompt"));
    }

    #[tokio::test]
    async fn cancellation_before_spawn() {
        let mut node = Node::new("cw", "Claude").with_handler("claude_workflow");
        node.config = serde_json::json!({ "prompt": "hi" });
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = handler().execute(&node, Outputs::new(), cancel).await;
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    #[test]
    fn parse_choice_maps_responses() {
        assert_eq!(parse_choice("allow"), ApprovalChoice::Allow);
        assert_eq!(parse_choice(" Yes\n"), ApprovalChoice::Allow);
        assert_eq!(parse_choice("1"), ApprovalChoice::Allow);
        assert_eq!(parse_choice("deny"), ApprovalChoice::Deny);
        assert_eq!(parse_choice("no"), ApprovalChoice::Deny);
        assert_eq!(parse_choice("2"), ApprovalChoice::Select(2));
        assert_eq!(parse_choice("garbage"), ApprovalChoice::Deny);
    }

    #[test]
    fn map_terminal_error_maps_variants() {
        assert!(matches!(
            map_terminal_error("n", TerminalError::Cancelled),
            NodeError::Cancelled { .. }
        ));
        assert!(matches!(
            map_terminal_error("n", TerminalError::Timeout(100, "x")),
            NodeError::Failed {
                recoverable: true,
                ..
            }
        ));
    }
}
