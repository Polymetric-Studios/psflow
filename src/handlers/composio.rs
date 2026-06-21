//! Composio handler — execute a Composio tool through the `composio` CLI.
//!
//! Ergonomic wrapper over `composio execute <slug> -d <json>`: the graph author
//! gives a tool slug and an `arguments` object, and the handler returns the
//! parsed response envelope as structured outputs instead of raw text. Auth is
//! whatever `composio login` established on the machine — no api key or
//! `user_id` lives in the graph. It only spawns a subprocess (no
//! `ExecutionContext`). It is an integration handler: registered solely by
//! psflow-run via `register_integrations`, never by the engine's
//! `with_defaults`, so it is unavailable on the stock `psflow` binary.

use crate::error::NodeError;
use crate::execute::blackboard::Blackboard;
use crate::execute::{CancellationToken, HandlerSchema, NodeHandler, Outputs, SchemaField};
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::template::TemplateResolver;
use std::collections::hash_map::DefaultHasher;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

const DEFAULT_BINARY: &str = "composio";
const DEFAULT_TIMEOUT_MS: u64 = 60_000;
const EXECUTE_SUBCOMMAND: &str = "execute";
const DATA_FLAG: &str = "-d";
const DRY_RUN_FLAG: &str = "--dry-run";

// Tool-response cache, switched on via env (set by the psflow-run runner). Lets
// graphs replay recorded responses offline and cache slow reads during dev.
const CACHE_DIR_ENV: &str = "PSFLOW_TOOL_CACHE_DIR";
const CACHE_MODE_ENV: &str = "PSFLOW_TOOL_CACHE_MODE";
const CACHE_TTL_ENV: &str = "PSFLOW_TOOL_CACHE_TTL_SECS";
const CACHE_MODE_REPLAY: &str = "replay";
const DEFAULT_CACHE_TTL_SECS: u64 = 86_400;

enum CacheMode {
    /// Use any cached response regardless of age (offline record/replay).
    Replay,
    /// Use a cached response only while it is younger than the TTL.
    Cache,
}

/// Filesystem cache of tool responses, keyed by (tool, arguments, dry_run).
struct ToolCache {
    dir: PathBuf,
    mode: CacheMode,
    ttl: Duration,
}

impl ToolCache {
    /// Active only when `PSFLOW_TOOL_CACHE_DIR` is set.
    fn from_env() -> Option<Self> {
        let dir = std::env::var_os(CACHE_DIR_ENV).map(PathBuf::from)?;
        let mode = match std::env::var(CACHE_MODE_ENV).ok().as_deref() {
            Some(CACHE_MODE_REPLAY) => CacheMode::Replay,
            _ => CacheMode::Cache,
        };
        let ttl = std::env::var(CACHE_TTL_ENV)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_CACHE_TTL_SECS);
        Some(Self {
            dir,
            mode,
            ttl: Duration::from_secs(ttl),
        })
    }

    fn path(&self, tool: &str, args_json: &str, dry_run: bool) -> PathBuf {
        let mut h = DefaultHasher::new();
        tool.hash(&mut h);
        args_json.hash(&mut h);
        dry_run.hash(&mut h);
        self.dir.join(format!("{tool}-{:016x}.json", h.finish()))
    }

    /// Return the cached stdout if present and (for `Cache` mode) still fresh.
    fn read(&self, path: &Path) -> Option<String> {
        if !path.exists() {
            return None;
        }
        match self.mode {
            CacheMode::Replay => std::fs::read_to_string(path).ok(),
            CacheMode::Cache => {
                let fresh = std::fs::metadata(path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| SystemTime::now().duration_since(t).ok())
                    .map(|age| age < self.ttl)
                    .unwrap_or(false);
                fresh.then(|| std::fs::read_to_string(path).ok()).flatten()
            }
        }
    }

    fn write(&self, path: &Path, stdout: &str) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, stdout);
    }
}

/// Executes a Composio tool via the `composio` CLI.
///
/// ## Configuration
///
/// - `config.tool` (required): the tool slug, e.g. `GOOGLESHEETS_SEARCH_SPREADSHEETS`.
///   Template-resolved.
/// - `config.arguments`: object of tool arguments (default `{}`). String leaves
///   are template-resolved, so `{inputs.spreadsheet_id}` works inside a value
///   without colliding with the JSON braces.
/// - `config.binary`: path to the CLI (default `composio`; must be on PATH).
/// - `config.timeout_ms`: kill the process after this many ms (default 60_000).
/// - `config.dry_run`: pass `--dry-run` to preview without executing (default false).
/// - `config.allow_unsuccessful`: when the envelope reports `successful: false`,
///   return it as outputs instead of failing the node (default false).
///
/// ## Outputs
///
/// - `successful` (Bool): the envelope's success flag.
/// - `data` (Value): the tool's result payload.
/// - `error` (String|Null): error message when unsuccessful.
/// - `log_id` (String): Composio execution log id (from `logId`), empty if absent.
pub struct ComposioHandler {
    resolver: Arc<dyn TemplateResolver>,
}

impl ComposioHandler {
    pub fn new(resolver: Arc<dyn TemplateResolver>) -> Self {
        Self { resolver }
    }
}

/// True when the string is exactly one `{…}` interpolation token with no
/// surrounding text (e.g. `"{ctx.max}"`), so its rendered value can be coerced
/// to a typed JSON scalar instead of staying a string.
fn is_whole_token(s: &str) -> bool {
    let t = s.trim();
    t.len() >= 3
        && t.starts_with('{')
        && t.ends_with('}')
        && !t[1..t.len() - 1].contains(['{', '}'])
}

/// Recursively template-resolve string leaves of an arguments value, leaving
/// structure (objects/arrays) and non-string scalars intact. This keeps `{...}`
/// interpolation usable inside values without touching the JSON braces.
///
/// Whole-value tokens (`"max_results": "{ctx.n}"`) whose rendered text parses as
/// a non-string JSON scalar (number/bool/null) are coerced to that type, so a
/// numeric input reaches a tool that wants an integer rather than a string.
fn render_arguments(
    value: &serde_json::Value,
    render: &dyn Fn(&str) -> Result<String, NodeError>,
) -> Result<serde_json::Value, NodeError> {
    match value {
        serde_json::Value::String(s) => {
            let rendered = render(s)?;
            if is_whole_token(s) {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&rendered) {
                    if !parsed.is_string() {
                        return Ok(parsed);
                    }
                }
            }
            Ok(serde_json::Value::String(rendered))
        }
        serde_json::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                out.push(render_arguments(item, render)?);
            }
            Ok(serde_json::Value::Array(out))
        }
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), render_arguments(v, render)?);
            }
            Ok(serde_json::Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

impl NodeHandler for ComposioHandler {
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
                    reason: "cancelled before composio execute".into(),
                });
            }

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

            let tool_template =
                config
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| NodeError::Failed {
                        source_message: None,
                        message: format!("node '{node_id}': missing config.tool"),
                        recoverable: false,
                    })?;
            let tool = render(tool_template)?;

            let arguments = match config.get("arguments") {
                Some(v) => render_arguments(v, &render)?,
                None => serde_json::json!({}),
            };
            let arguments_json =
                serde_json::to_string(&arguments).map_err(|e| NodeError::Failed {
                    source_message: Some(e.to_string()),
                    message: format!("node '{node_id}': cannot serialize arguments: {e}"),
                    recoverable: false,
                })?;

            let binary = config
                .get("binary")
                .and_then(|v| v.as_str())
                .unwrap_or(DEFAULT_BINARY);
            let timeout_ms = config
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_MS);
            let dry_run = config
                .get("dry_run")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let allow_unsuccessful = config
                .get("allow_unsuccessful")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            // Cache/replay layer (off unless PSFLOW_TOOL_CACHE_DIR is set).
            let cache = ToolCache::from_env();
            let cache_path = cache
                .as_ref()
                .map(|c| c.path(&tool, &arguments_json, dry_run));
            let cached = match (&cache, &cache_path) {
                (Some(c), Some(p)) => c.read(p),
                _ => None,
            };

            let stdout = if let Some(s) = cached {
                eprintln!("[composio][{node_id}] cache hit for {tool}");
                s
            } else {
                let mut cmd = tokio::process::Command::new(binary);
                cmd.arg(EXECUTE_SUBCOMMAND).arg(&tool);
                if dry_run {
                    cmd.arg(DRY_RUN_FLAG);
                }
                cmd.arg(DATA_FLAG).arg(&arguments_json);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::piped());
                cmd.kill_on_drop(true);

                let child = cmd.spawn().map_err(|e| NodeError::Failed {
                    source_message: Some(e.to_string()),
                    message: format!("node '{node_id}': failed to spawn '{binary}': {e}"),
                    recoverable: false,
                })?;

                let wait = child.wait_with_output();
                let output = tokio::select! {
                    res = tokio::time::timeout(Duration::from_millis(timeout_ms), wait) => {
                        match res {
                            Ok(Ok(out)) => out,
                            Ok(Err(e)) => return Err(NodeError::Failed {
                                source_message: Some(e.to_string()),
                                message: format!("node '{node_id}': composio wait failed: {e}"),
                                recoverable: false,
                            }),
                            Err(_) => return Err(NodeError::Failed {
                                source_message: None,
                                message: format!(
                                    "node '{node_id}': composio execute '{tool}' timed out after {timeout_ms}ms"
                                ),
                                recoverable: true,
                            }),
                        }
                    }
                    _ = cancel.cancelled() => return Err(NodeError::Cancelled {
                        reason: "cancelled during composio execute".into(),
                    }),
                };

                let exit_code = output.status.code().unwrap_or(-1);
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

                if exit_code != 0 {
                    return Err(NodeError::Failed {
                        source_message: Some(stderr.clone()),
                        message: format!(
                            "node '{node_id}': composio execute '{tool}' exited with code {exit_code}: {}",
                            stderr.trim()
                        ),
                        recoverable: false,
                    });
                }

                if let (Some(c), Some(p)) = (&cache, &cache_path) {
                    c.write(p, &stdout);
                }
                stdout
            };

            // The CLI prints the JSON envelope on stdout; the update banner goes
            // to stderr, so stdout parses cleanly.
            let envelope: serde_json::Value =
                serde_json::from_str(stdout.trim()).map_err(|e| NodeError::Failed {
                    source_message: Some(e.to_string()),
                    message: format!(
                        "node '{node_id}': composio output was not JSON: {e}; first 200 bytes: {:?}",
                        stdout.chars().take(200).collect::<String>()
                    ),
                    recoverable: false,
                })?;

            let successful = envelope
                .get("successful")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let error_msg = envelope
                .get("error")
                .and_then(|v| v.as_str())
                .map(String::from);

            if !successful && !allow_unsuccessful {
                return Err(NodeError::Failed {
                    source_message: error_msg.clone(),
                    message: format!(
                        "node '{node_id}': composio tool '{tool}' was unsuccessful: {}",
                        error_msg.as_deref().unwrap_or("(no error message)")
                    ),
                    recoverable: false,
                });
            }

            // `logId` (camelCase) from the CLI; tolerate `log_id` too.
            let log_id = envelope
                .get("logId")
                .or_else(|| envelope.get("log_id"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let data = envelope
                .get("data")
                .cloned()
                .unwrap_or(serde_json::Value::Null);

            let mut outputs = Outputs::new();
            outputs.insert("successful".into(), Value::Bool(successful));
            outputs.insert("data".into(), Value::from(data));
            outputs.insert(
                "error".into(),
                error_msg.map(Value::String).unwrap_or(Value::Null),
            );
            outputs.insert("log_id".into(), Value::String(log_id));
            Ok(outputs)
        })
    }

    fn schema(&self, name: &str) -> HandlerSchema {
        HandlerSchema::new(name, "Execute a Composio tool via the composio CLI")
            .with_config(SchemaField::new("tool", "string").required().describe(
                "Composio tool slug, e.g. GOOGLESHEETS_SEARCH_SPREADSHEETS. Template-resolved.",
            ))
            .with_config(
                SchemaField::new("arguments", "object")
                    .describe("Tool arguments. String leaves are template-resolved.")
                    .default(serde_json::json!({})),
            )
            .with_config(
                SchemaField::new("binary", "string")
                    .describe("Path to the composio CLI (must be on PATH)")
                    .default(serde_json::json!(DEFAULT_BINARY)),
            )
            .with_config(
                SchemaField::new("timeout_ms", "integer")
                    .describe("Kill after this many milliseconds")
                    .default(serde_json::json!(DEFAULT_TIMEOUT_MS)),
            )
            .with_config(
                SchemaField::new("dry_run", "boolean")
                    .describe("Preview the call with --dry-run instead of executing")
                    .default(serde_json::json!(false)),
            )
            .with_config(
                SchemaField::new("allow_unsuccessful", "boolean")
                    .describe("Return successful:false as outputs instead of failing the node")
                    .default(serde_json::json!(false)),
            )
            .with_output(SchemaField::new("successful", "boolean"))
            .with_output(SchemaField::new("data", "object"))
            .with_output(SchemaField::new("error", "string"))
            .with_output(SchemaField::new("log_id", "string"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::PromptTemplateResolver;

    fn render_with(inputs: &Outputs) -> impl Fn(&str) -> Result<String, NodeError> + '_ {
        let resolver = PromptTemplateResolver;
        let bb = Blackboard::new();
        move |tpl: &str| {
            resolver
                .render(tpl, inputs, &bb)
                .map_err(|e| NodeError::Failed {
                    source_message: None,
                    message: e.to_string(),
                    recoverable: false,
                })
        }
    }

    #[test]
    fn render_arguments_templates_string_leaves_only() {
        let mut inputs = Outputs::new();
        inputs.insert("sid".into(), Value::String("sheet-123".into()));
        let render = render_with(&inputs);
        let args = serde_json::json!({
            "spreadsheet_id": "{inputs.sid}",
            "ranges": ["A1:B2", "{inputs.sid}"],
            "count": 5,
            "flag": true
        });
        let out = render_arguments(&args, &render).unwrap();
        assert_eq!(out["spreadsheet_id"], serde_json::json!("sheet-123"));
        assert_eq!(out["ranges"][1], serde_json::json!("sheet-123"));
        assert_eq!(out["count"], serde_json::json!(5));
        assert_eq!(out["flag"], serde_json::json!(true));
    }

    #[test]
    fn whole_token_coerces_to_typed_scalar() {
        let mut inputs = Outputs::new();
        inputs.insert("n".into(), Value::I64(7));
        inputs.insert("name".into(), Value::String("INV".into()));
        let render = render_with(&inputs);
        let args = serde_json::json!({
            "max_results": "{inputs.n}",        // whole token, numeric -> 7 (number)
            "query": "{inputs.name}",            // whole token, non-JSON -> "INV" (string)
            "label": "id-{inputs.n}"             // not a whole token -> "id-7" (string)
        });
        let out = render_arguments(&args, &render).unwrap();
        assert_eq!(out["max_results"], serde_json::json!(7));
        assert_eq!(out["query"], serde_json::json!("INV"));
        assert_eq!(out["label"], serde_json::json!("id-7"));
    }

    #[tokio::test]
    async fn missing_tool_errors() {
        let node = Node::new("c", "Composio").with_handler("composio");
        let handler = ComposioHandler::new(Arc::new(PromptTemplateResolver));
        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing config.tool"));
    }

    #[tokio::test]
    async fn non_json_output_is_reported() {
        // Use `echo` as the "CLI": it prints the args, which are not JSON, so
        // the handler should surface a clear parse error.
        let mut node = Node::new("c", "Composio").with_handler("composio");
        node.config = serde_json::json!({ "tool": "X", "binary": "echo" });
        let handler = ComposioHandler::new(Arc::new(PromptTemplateResolver));
        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("was not JSON"));
    }
}
