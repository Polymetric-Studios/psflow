//! Shell command handler — executes external processes.
//!
//! Provides template-resolved command + args execution with captured
//! stdout/stderr/exit code, optional cwd/env, timeout, and an explicit opt-in
//! to shell interpretation.

use crate::error::NodeError;
use crate::execute::blackboard::Blackboard;
use crate::execute::{CancellationToken, HandlerSchema, NodeHandler, Outputs, SchemaField};
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::template::TemplateResolver;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Shell command execution handler.
///
/// Executes an external command with captured output. By default uses direct
/// process spawning (no shell interpretation), which is the safer option for
/// template-resolved command strings. Set `config.use_shell: true` to run the
/// command through `sh -c` instead — use with care.
///
/// ## Configuration
///
/// - `config.command` (required): command template. Resolved against
///   [`TemplateResolver`] with node inputs + blackboard.
/// - `config.args`: array of argument templates. Resolved one-by-one. Ignored
///   when `use_shell` is true (pass everything in `command` instead).
/// - `config.cwd`: working directory. Template-resolved.
/// - `config.env`: map of env-var name → template. Additive to the parent
///   environment; does not clear existing vars.
/// - `config.timeout_ms`: kill the process after this many milliseconds
///   (default: 30_000).
/// - `config.use_shell`: run via `sh -c` for shell syntax (default: false).
/// - `config.allow_nonzero_exit`: do not treat a non-zero exit as a handler
///   error (default: false). Useful when callers want the exit code as data.
///
/// ## Outputs
///
/// - `stdout`: captured standard output (String).
/// - `stderr`: captured standard error (String).
/// - `exit_code`: process exit code (I64). `-1` when the process was killed
///   by a signal and no code is available.
///
/// ## Security
///
/// Default mode (no `use_shell`) passes the command name and args directly to
/// the OS without a shell interpreter, so template-resolved strings cannot
/// inject additional commands via `;` or `|`. When `use_shell: true`,
/// template content is interpreted by `sh` and the caller is responsible for
/// escaping.
pub struct ShellHandler {
    resolver: Arc<dyn TemplateResolver>,
}

impl ShellHandler {
    pub fn new(resolver: Arc<dyn TemplateResolver>) -> Self {
        Self { resolver }
    }
}

impl NodeHandler for ShellHandler {
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
                    reason: "cancelled before shell command".into(),
                });
            }

            let command_template = config
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': missing config.command"),
                    recoverable: false,
                })?
                .to_string();

            let args_templates: Vec<String> = config
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            let cwd_template: Option<String> =
                config.get("cwd").and_then(|v| v.as_str()).map(String::from);

            let env_templates: BTreeMap<String, String> = config
                .get("env")
                .and_then(|v| v.as_object())
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();

            let timeout_ms = config
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(30_000);

            let use_shell = config
                .get("use_shell")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let allow_nonzero_exit = config
                .get("allow_nonzero_exit")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            // Resolve templates — empty blackboard placeholder; handlers running
            // inside the executor see the live blackboard via the resolver if
            // the embedder's resolver threads it. psflow's default resolver
            // supports `{ctx.*}` when blackboard is non-empty, but here we
            // haven't got a context handle, so pass a fresh blackboard.
            //
            // Embedders that need blackboard access in shell templates should
            // hold their own resolver that closes over a context reference.
            let bb = Blackboard::new();
            let render = |tpl: &str| -> Result<String, NodeError> {
                resolver
                    .render(tpl, &inputs, &bb)
                    .map_err(|e| NodeError::Failed {
                        source_message: Some(e.to_string()),
                        message: format!("node '{node_id}': template error: {e}"),
                        recoverable: false,
                    })
            };

            let command = render(&command_template)?;
            let args: Vec<String> = args_templates
                .iter()
                .map(|t| render(t))
                .collect::<Result<_, _>>()?;
            let cwd = cwd_template.as_deref().map(render).transpose()?;
            let mut env_vars = BTreeMap::new();
            for (k, v) in &env_templates {
                env_vars.insert(k.clone(), render(v)?);
            }

            // Build command
            let mut cmd = if use_shell {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(&command);
                c
            } else {
                let mut c = tokio::process::Command::new(&command);
                c.args(&args);
                c
            };

            if let Some(dir) = cwd.as_deref() {
                cmd.current_dir(dir);
            }
            for (k, v) in &env_vars {
                cmd.env(k, v);
            }

            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            cmd.kill_on_drop(true);

            let child = cmd.spawn().map_err(|e| NodeError::Failed {
                source_message: Some(e.to_string()),
                message: format!("node '{node_id}': failed to spawn '{command}': {e}"),
                recoverable: false,
            })?;

            let wait = child.wait_with_output();
            let output = tokio::select! {
                res = tokio::time::timeout(Duration::from_millis(timeout_ms), wait) => {
                    match res {
                        Ok(Ok(out)) => out,
                        Ok(Err(e)) => {
                            return Err(NodeError::Failed {
                                source_message: Some(e.to_string()),
                                message: format!("node '{node_id}': shell wait failed: {e}"),
                                recoverable: false,
                            });
                        }
                        Err(_) => {
                            return Err(NodeError::Failed {
                                source_message: None,
                                message: format!(
                                    "node '{node_id}': shell command '{command}' timed out after {timeout_ms}ms"
                                ),
                                recoverable: true,
                            });
                        }
                    }
                }
                _ = cancel.cancelled() => {
                    return Err(NodeError::Cancelled {
                        reason: "cancelled during shell command".into(),
                    });
                }
            };

            let exit_code: i64 = output.status.code().map(|c| c as i64).unwrap_or(-1);
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

            if !allow_nonzero_exit && exit_code != 0 {
                return Err(NodeError::Failed {
                    source_message: Some(stderr.clone()),
                    message: format!(
                        "node '{node_id}': shell command '{command}' exited with code {exit_code}"
                    ),
                    recoverable: false,
                });
            }

            let mut outputs = Outputs::new();
            outputs.insert("stdout".into(), Value::String(stdout));
            outputs.insert("stderr".into(), Value::String(stderr));
            outputs.insert("exit_code".into(), Value::I64(exit_code));
            Ok(outputs)
        })
    }

    fn schema(&self, name: &str) -> HandlerSchema {
        HandlerSchema::new(name, "Execute an external shell command")
            .with_config(
                SchemaField::new("command", "string")
                    .required()
                    .describe("Command to execute. Template-resolved."),
            )
            .with_config(
                SchemaField::new("args", "array<string>")
                    .describe("Argument templates (ignored when use_shell=true)"),
            )
            .with_config(SchemaField::new("cwd", "string").describe("Working directory template"))
            .with_config(
                SchemaField::new("env", "map<string,string>")
                    .describe("Additional environment variables (templates)"),
            )
            .with_config(
                SchemaField::new("timeout_ms", "integer")
                    .describe("Kill after this many milliseconds")
                    .default(serde_json::json!(30_000)),
            )
            .with_config(
                SchemaField::new("use_shell", "boolean")
                    .describe("Run via `sh -c` instead of direct exec")
                    .default(serde_json::json!(false)),
            )
            .with_config(
                SchemaField::new("allow_nonzero_exit", "boolean")
                    .describe("Do not treat non-zero exit as a handler error")
                    .default(serde_json::json!(false)),
            )
            .with_output(SchemaField::new("stdout", "string"))
            .with_output(SchemaField::new("stderr", "string"))
            .with_output(SchemaField::new("exit_code", "integer"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::PromptTemplateResolver;

    fn handler() -> ShellHandler {
        ShellHandler::new(Arc::new(PromptTemplateResolver))
    }

    #[tokio::test]
    async fn runs_simple_command() {
        let mut node = Node::new("sh", "Shell").with_handler("shell");
        node.config = serde_json::json!({
            "command": "echo",
            "args": ["hello"],
        });
        let out = handler()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        match out.get("stdout").unwrap() {
            Value::String(s) => assert!(s.contains("hello")),
            other => panic!("expected stdout string, got {other:?}"),
        }
        assert_eq!(out.get("exit_code"), Some(&Value::I64(0)));
    }

    #[tokio::test]
    async fn missing_command_errors() {
        let node = Node::new("sh", "Shell").with_handler("shell");
        let result = handler()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing config.command"));
    }

    #[tokio::test]
    async fn nonzero_exit_errors_by_default() {
        let mut node = Node::new("sh", "Shell").with_handler("shell");
        node.config = serde_json::json!({
            "command": "sh",
            "args": ["-c", "exit 7"],
        });
        let result = handler()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("exited with code 7"));
    }

    #[tokio::test]
    async fn nonzero_exit_allowed_returns_code() {
        let mut node = Node::new("sh", "Shell").with_handler("shell");
        node.config = serde_json::json!({
            "command": "sh",
            "args": ["-c", "exit 3"],
            "allow_nonzero_exit": true,
        });
        let out = handler()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.get("exit_code"), Some(&Value::I64(3)));
    }

    #[tokio::test]
    async fn use_shell_interprets_pipes() {
        let mut node = Node::new("sh", "Shell").with_handler("shell");
        node.config = serde_json::json!({
            "command": "echo hello | tr a-z A-Z",
            "use_shell": true,
        });
        let out = handler()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("stdout").unwrap() {
            Value::String(s) => assert!(s.contains("HELLO")),
            other => panic!("expected stdout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn template_in_command_resolves_from_inputs() {
        let mut node = Node::new("sh", "Shell").with_handler("shell");
        node.config = serde_json::json!({
            "command": "echo",
            "args": ["{inputs.greeting}"],
        });
        let mut inputs = Outputs::new();
        inputs.insert("greeting".into(), Value::String("hola".into()));
        let out = handler()
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();
        match out.get("stdout").unwrap() {
            Value::String(s) => assert!(s.contains("hola"), "got {s:?}"),
            other => panic!("expected stdout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_kills_slow_command() {
        let mut node = Node::new("sh", "Shell").with_handler("shell");
        node.config = serde_json::json!({
            "command": "sleep",
            "args": ["5"],
            "timeout_ms": 100,
        });
        let result = handler()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn cancellation_before_spawn() {
        let mut node = Node::new("sh", "Shell").with_handler("shell");
        node.config = serde_json::json!({ "command": "echo", "args": ["hi"] });
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = handler().execute(&node, Outputs::new(), cancel).await;
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    #[tokio::test]
    async fn cwd_is_applied() {
        use std::env;
        let tempdir = tempfile::tempdir().unwrap();
        let path = tempdir.path().to_path_buf();
        let mut node = Node::new("sh", "Shell").with_handler("shell");
        node.config = serde_json::json!({
            "command": "pwd",
            "cwd": path.to_string_lossy(),
        });
        let out = handler()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        let _ = env::current_dir();
        match out.get("stdout").unwrap() {
            Value::String(s) => {
                // macOS canonicalizes /var -> /private/var; compare by component
                // suffix to avoid a brittle equality check.
                let canon = std::fs::canonicalize(&path).unwrap();
                assert!(
                    s.contains(canon.to_string_lossy().as_ref()),
                    "stdout {s:?} does not contain cwd {canon:?}"
                );
            }
            other => panic!("expected stdout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn env_vars_are_injected() {
        let mut node = Node::new("sh", "Shell").with_handler("shell");
        node.config = serde_json::json!({
            "command": "sh",
            "args": ["-c", "echo $PSFLOW_TEST_VAR"],
            "env": { "PSFLOW_TEST_VAR": "secret-value" },
        });
        let out = handler()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("stdout").unwrap() {
            Value::String(s) => assert!(s.contains("secret-value"), "got {s:?}"),
            other => panic!("expected stdout, got {other:?}"),
        }
    }
}
