//! psflow-run — personal runner for named psflow graphs.
//!
//! Adds the infrastructure the stock `psflow` binary lacks for unattended,
//! reusable automation:
//!
//! - **Named graphs**: `psflow-run daily-digest` resolves `<graphs-dir>/daily-digest.mmd`.
//! - **Runtime inputs**: `--input k=v` seeds a parent blackboard; graphs read `{ctx.k}`.
//! - **LLM adapter wired**: registers `llm_call` with the keyless `ClaudeCliAdapter`,
//!   so Composio-tool -> LLM workflows run here (the stock binary omits `llm_call`).
//! - **Run history**: writes an execution trace + summary (incl. Composio `log_id`s)
//!   to `<runs-dir>` per run.
//! - **Notify-on-failure**: on any failed node, runs an `on-failure` graph if present
//!   (passing the error as inputs), and posts a desktop notification.
//!
//! Auth for Composio tools is whatever `composio login` established; auth for the
//! LLM is the `claude` CLI. No api keys live here.

use clap::Parser;
use psflow::adapter::ClaudeCliAdapter;
use psflow::execute::ContextInheritance;
use psflow::scripting::engine::ScriptEngine;
use psflow::{
    load_mermaid, Blackboard, BlackboardScope, ExecutionResult, LlmCallHandler, NodeRegistry,
    NodeState, Outputs, PromptTemplateResolver, TemplateError, TemplateResolver,
    TopologicalExecutor, Value,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_GRAPHS_DIR: &str = "graphs";
const DEFAULT_RUNS_DIR: &str = "runs";
const ON_FAILURE_GRAPH: &str = "on-failure";

/// Template resolver that exposes runtime `--input` values to handler templates
/// as `{ctx.key}` (and bare `{key}`). Stateless handlers (composio, shell, …)
/// render against a fresh blackboard, so seeding the executor's blackboard is
/// not enough — this resolver merges the runtime inputs into whatever
/// blackboard the handler passes, then delegates to the default engine.
struct RuntimeInputResolver {
    inputs: BTreeMap<String, Value>,
    inner: PromptTemplateResolver,
}

impl TemplateResolver for RuntimeInputResolver {
    fn render(
        &self,
        template: &str,
        inputs: &Outputs,
        blackboard: &Blackboard,
    ) -> Result<String, TemplateError> {
        let mut merged = blackboard.clone();
        for (k, v) in &self.inputs {
            merged.set(k.clone(), v.clone(), BlackboardScope::Global);
        }
        self.inner.render(template, inputs, &merged)
    }
}

#[derive(Parser)]
#[command(name = "psflow-run", about = "Run a named psflow graph with inputs")]
struct Cli {
    /// Graph name (resolved to <graphs-dir>/<name>.mmd) or a path to a .mmd file.
    graph: String,

    /// Runtime input as key=value. Repeatable. Values parse as JSON, else string.
    /// Graphs read these via `{ctx.key}`.
    #[arg(short, long = "input", value_name = "KEY=VALUE")]
    inputs: Vec<String>,

    /// Directory holding named graphs (env PSFLOW_GRAPHS_DIR overrides; default ./graphs).
    #[arg(long)]
    graphs_dir: Option<PathBuf>,

    /// Directory for run-history records (default ./runs).
    #[arg(long)]
    runs_dir: Option<PathBuf>,

    /// Do not run the on-failure hook / desktop notification on failure.
    #[arg(long)]
    no_notify: bool,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let graphs_dir = cli
        .graphs_dir
        .clone()
        .or_else(|| std::env::var_os("PSFLOW_GRAPHS_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(DEFAULT_GRAPHS_DIR));
    let runs_dir = cli
        .runs_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_RUNS_DIR));

    let inputs = match parse_inputs(&cli.inputs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(run(
        &cli.graph,
        &graphs_dir,
        &runs_dir,
        inputs,
        cli.no_notify,
    )) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Returns Ok(true) on success, Ok(false) when a node failed.
async fn run(
    graph_ref: &str,
    graphs_dir: &Path,
    runs_dir: &Path,
    inputs: BTreeMap<String, Value>,
    no_notify: bool,
) -> Result<bool, String> {
    let path = resolve_graph_path(graph_ref, graphs_dir)?;
    let graph_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(graph_ref)
        .to_string();

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let graph = load_mermaid(&content).map_err(|errs| {
        errs.iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join("; ")
    })?;

    // Seed runtime inputs into a parent blackboard; graphs read them as {ctx.key}.
    let mut parent_bb = Blackboard::new();
    for (k, v) in &inputs {
        parent_bb.set(k.clone(), v.clone(), BlackboardScope::Global);
    }

    let adapter = Arc::new(ClaudeCliAdapter::new());
    let handlers = build_handlers(adapter.clone(), inputs.clone());
    let executor = TopologicalExecutor::new().with_adapter(adapter);

    eprintln!(
        "running '{graph_name}' ({} nodes) with {} input(s)",
        graph.node_count(),
        inputs.len()
    );
    let result = executor
        .execute_with_parent(&graph, &handlers, &parent_bb, ContextInheritance::ReadOnly)
        .await
        .map_err(|e| format!("execution error: {e}"))?;

    let failed = report_states(&result);
    let record_path =
        write_run_record(runs_dir, &graph_name, &result, &failed).unwrap_or_else(|e| {
            eprintln!("warning: could not write run record: {e}");
            PathBuf::new()
        });
    if !record_path.as_os_str().is_empty() {
        eprintln!("run record: {}", record_path.display());
    }

    if failed.is_empty() {
        eprintln!(
            "completed in {:.1}ms",
            result.elapsed.as_secs_f64() * 1000.0
        );
        Ok(true)
    } else {
        eprintln!("FAILED nodes: {}", failed.join(", "));
        if !no_notify {
            notify_failure(graphs_dir, &graph_name, &failed, &result).await;
        }
        Ok(false)
    }
}

/// Build the handler registry: stateless defaults (incl. `composio`) wired to a
/// resolver that exposes runtime inputs as `{ctx.key}`, plus `llm_call` on the
/// supplied adapter.
fn build_handlers(
    adapter: Arc<ClaudeCliAdapter>,
    inputs: BTreeMap<String, Value>,
) -> psflow::execute::HandlerRegistry {
    let engine = Arc::new(ScriptEngine::with_defaults());
    let resolver = Arc::new(RuntimeInputResolver {
        inputs,
        inner: PromptTemplateResolver,
    });
    let mut reg = NodeRegistry::with_defaults_and_resolver(engine, resolver);
    reg.register("llm_call", Arc::new(LlmCallHandler::new(adapter)));
    reg.into_handler_registry()
}

fn resolve_graph_path(graph_ref: &str, graphs_dir: &Path) -> Result<PathBuf, String> {
    let direct = PathBuf::from(graph_ref);
    if direct.extension().is_some() && direct.exists() {
        return Ok(direct);
    }
    let named = graphs_dir.join(format!("{graph_ref}.mmd"));
    if named.exists() {
        return Ok(named);
    }
    Err(format!(
        "graph '{graph_ref}' not found (looked for '{}' and '{}')",
        direct.display(),
        named.display()
    ))
}

fn parse_inputs(raw: &[String]) -> Result<BTreeMap<String, Value>, String> {
    let mut out = BTreeMap::new();
    for item in raw {
        let (k, v) = item
            .split_once('=')
            .ok_or_else(|| format!("invalid --input '{item}', expected key=value"))?;
        // Try JSON (numbers, bools, objects, arrays); fall back to a plain string.
        let value = match serde_json::from_str::<serde_json::Value>(v) {
            Ok(json) => Value::from(json),
            Err(_) => Value::String(v.to_string()),
        };
        out.insert(k.to_string(), value);
    }
    Ok(out)
}

fn report_states(result: &ExecutionResult) -> Vec<String> {
    let mut states: Vec<_> = result.node_states.iter().collect();
    states.sort_by_key(|(id, _)| (*id).clone());
    for (id, state) in &states {
        let symbol = match state {
            NodeState::Completed => "+",
            NodeState::Failed => "!",
            NodeState::Cancelled => "~",
            _ => "?",
        };
        eprintln!("  [{symbol}] {id}: {state}");
    }
    result
        .node_states
        .iter()
        .filter(|(_, s)| **s == NodeState::Failed)
        .map(|(id, _)| id.clone())
        .collect()
}

/// Composio handler outputs carry a `log_id`; collect non-empty ones for forensics.
fn collect_log_ids(result: &ExecutionResult) -> BTreeMap<String, String> {
    let mut ids = BTreeMap::new();
    for (node_id, outputs) in &result.node_outputs {
        if let Some(Value::String(s)) = outputs.get("log_id") {
            if !s.is_empty() {
                ids.insert(node_id.clone(), s.clone());
            }
        }
    }
    ids
}

fn epoch_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn write_run_record(
    runs_dir: &Path,
    graph_name: &str,
    result: &ExecutionResult,
    failed: &[String],
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(runs_dir).map_err(|e| e.to_string())?;
    let ts = epoch_millis();
    let summary = serde_json::json!({
        "graph": graph_name,
        "timestamp_ms": ts,
        "status": if failed.is_empty() { "ok" } else { "failed" },
        "elapsed_ms": result.elapsed.as_secs_f64() * 1000.0,
        "failed_nodes": failed,
        "states": result.node_states.iter()
            .map(|(id, s)| (id.clone(), s.to_string())).collect::<BTreeMap<_, _>>(),
        "composio_log_ids": collect_log_ids(result),
        "trace": result.trace(),
    });
    let path = runs_dir.join(format!("{ts}-{graph_name}.json"));
    let json = serde_json::to_string_pretty(&summary).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(path)
}

/// On failure: run an `on-failure` graph if present (best-effort, no recursion),
/// then post a desktop notification.
async fn notify_failure(
    graphs_dir: &Path,
    graph_name: &str,
    failed: &[String],
    result: &ExecutionResult,
) {
    let summary = format!("graph '{graph_name}' failed: {}", failed.join(", "));

    let hook = graphs_dir.join(format!("{ON_FAILURE_GRAPH}.mmd"));
    if hook.exists() {
        eprintln!("running on-failure hook: {}", hook.display());
        let mut hook_inputs = BTreeMap::new();
        hook_inputs.insert("graph".to_string(), Value::String(graph_name.to_string()));
        hook_inputs.insert("failed_nodes".to_string(), Value::String(failed.join(", ")));
        if let Some((_, msg)) = first_failure_message(result) {
            hook_inputs.insert("error".to_string(), Value::String(msg));
        }
        let mut bb = Blackboard::new();
        for (k, v) in &hook_inputs {
            bb.set(k.clone(), v.clone(), BlackboardScope::Global);
        }
        if let Ok(content) = std::fs::read_to_string(&hook) {
            if let Ok(g) = load_mermaid(&content) {
                let handlers = build_handlers(Arc::new(ClaudeCliAdapter::new()), hook_inputs);
                // Best-effort; do not re-notify if the hook itself fails.
                let _ = TopologicalExecutor::new()
                    .execute_with_parent(&g, &handlers, &bb, ContextInheritance::ReadOnly)
                    .await;
            }
        }
    }

    // Desktop notification (macOS). Best-effort; ignored on other platforms.
    let script = format!("display notification {:?} with title \"psflow\"", summary);
    let _ = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
        .await;
}

fn first_failure_message(result: &ExecutionResult) -> Option<(String, String)> {
    use psflow::execute::ExecutionEvent;
    result.events.iter().find_map(|ev| match ev {
        ExecutionEvent::NodeFailed { node_id, error } => Some((node_id.clone(), error.to_string())),
        _ => None,
    })
}
