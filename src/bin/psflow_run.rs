//! psflow-run — personal runner for named psflow graphs.
//!
//! Adds the infrastructure the stock `psflow` binary lacks for unattended,
//! reusable automation:
//!
//! - **Named graphs**: `psflow-run daily-digest` resolves `<graphs-dir>/daily-digest.mmd`.
//! - **Runtime inputs**: `--input k=v` seeds a parent blackboard; graphs read `{ctx.k}`.
//! - **LLM adapter wired**: registers `llm_call` with the keyless `ClaudeCliAdapter`,
//!   so tool -> LLM workflows run here (the stock binary omits `llm_call`).
//! - **Run history**: writes an execution trace + summary (incl. any `log_id`s)
//!   to `<runs-dir>` per run.
//! - **Notify-on-failure**: on any failed node, runs an `on-failure` graph if present
//!   (passing the error as inputs), and posts a desktop notification.
//!
//! The engine is provider-neutral; optional third-party integrations (e.g.
//! Composio) are registered separately in `register_integrations` so they can be
//! dropped without touching the core. LLM auth is the `claude` CLI; no api keys
//! live here.

use clap::Parser;
use psflow::adapter::ClaudeCliAdapter;
use psflow::execute::{ContextInheritance, ExecutionContext};
use psflow::handlers::{GraphLibrary, MapHandler, SubgraphInvocationHandler};
use psflow::scripting::engine::ScriptEngine;
use psflow::{
    load_mermaid, Blackboard, BlackboardScope, ExecutionResult, LlmCallHandler, NodeRegistry,
    NodeState, Outputs, PromptTemplateResolver, RhaiHandler, TemplateError, TemplateResolver,
    TopologicalExecutor, Value,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, BufReader};

const DEFAULT_GRAPHS_DIR: &str = "graphs";
const DEFAULT_RUNS_DIR: &str = "runs";
const DEFAULT_STATE_DIR: &str = "state";
const ON_FAILURE_GRAPH: &str = "on-failure";
const CONFIG_FILE: &str = "config.json";
const CTX_MARKER: &str = "{ctx.";
/// Node outputs with this prefix are persisted to the graph's cross-run state
/// (prefix stripped), e.g. an output `save_last_seen` becomes state `last_seen`.
const STATE_SAVE_PREFIX: &str = "save_";
const DEFAULT_CACHE_DIR: &str = "cache/tools";

/// Template resolver that exposes runtime `--input` values to handler templates
/// as `{ctx.key}` (and bare `{key}`). Stateless handlers (e.g. `shell`)
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
    /// Optional when --list is given.
    graph: Option<String>,

    /// Runtime input as key=value. Repeatable. Values parse as JSON, else string.
    /// Graphs read these via `{ctx.key}`. Overrides config-file defaults.
    #[arg(short, long = "input", value_name = "KEY=VALUE")]
    inputs: Vec<String>,

    /// List available named graphs and exit.
    #[arg(long)]
    list: bool,

    /// Directory holding named graphs (env PSFLOW_GRAPHS_DIR overrides; default ./graphs).
    #[arg(long)]
    graphs_dir: Option<PathBuf>,

    /// Directory for run-history records (default ./runs).
    #[arg(long)]
    runs_dir: Option<PathBuf>,

    /// Directory for cross-run state files (default ./state).
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Do not run the on-failure hook / desktop notification on failure.
    #[arg(long)]
    no_notify: bool,

    /// Replay recorded tool responses offline; records on cache miss.
    #[arg(long)]
    replay: bool,

    /// Cache tool responses with a TTL (default 86400s).
    #[arg(long)]
    cache: bool,

    /// Cache TTL in seconds (with --cache).
    #[arg(long)]
    cache_ttl: Option<u64>,

    /// Directory for the tool-response cache (default ./cache/tools).
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Listen mode: run the command in PSFLOW_LISTEN_CMD (any provider's event
    /// stream) and run the named graph once per emitted JSON line, with the
    /// event JSON as `{ctx.event}`.
    #[arg(long)]
    listen: bool,
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
    let state_dir = cli
        .state_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR));

    if cli.list {
        list_graphs(&graphs_dir);
        return ExitCode::SUCCESS;
    }

    let Some(graph_ref) = cli.graph.clone() else {
        eprintln!("error: a graph name is required (or use --list)");
        return ExitCode::FAILURE;
    };

    let cli_inputs = match parse_inputs(&cli.inputs) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Tool handlers that support caching read these env vars (provider-neutral).
    if cli.replay || cli.cache {
        let cache_dir = cli
            .cache_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CACHE_DIR));
        std::env::set_var("PSFLOW_TOOL_CACHE_DIR", &cache_dir);
        if cli.replay {
            std::env::set_var("PSFLOW_TOOL_CACHE_MODE", "replay");
        } else {
            std::env::set_var("PSFLOW_TOOL_CACHE_MODE", "cache");
            if let Some(ttl) = cli.cache_ttl {
                std::env::set_var("PSFLOW_TOOL_CACHE_TTL_SECS", ttl.to_string());
            }
        }
    }

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");

    if cli.listen {
        return match rt.block_on(listen_loop(
            &graph_ref,
            &graphs_dir,
            &runs_dir,
            &state_dir,
            cli_inputs,
            cli.no_notify,
        )) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    match rt.block_on(run(
        &graph_ref,
        &graphs_dir,
        &runs_dir,
        &state_dir,
        cli_inputs,
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

/// The command whose stdout produces one JSON event object per line. Provider
/// -agnostic: set it via `PSFLOW_LISTEN_CMD` (e.g. a Composio SDK subscriber, or
/// any other source). No built-in default, so the bridge has no provider baked in.
fn listen_command() -> Result<String, String> {
    std::env::var("PSFLOW_LISTEN_CMD").map_err(|_| {
        "listen mode requires PSFLOW_LISTEN_CMD (the event-stream command)".to_string()
    })
}

/// Merge a trigger event (raw JSON line) into a copy of the base inputs as
/// `event`. Returns None when the line is not a JSON object (banners, blanks).
fn event_to_inputs(line: &str, base: &BTreeMap<String, Value>) -> Option<BTreeMap<String, Value>> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(line).ok()?;
    let mut inputs = base.clone();
    inputs.insert("event".to_string(), Value::String(line.to_string()));
    Some(inputs)
}

/// Stream events from `PSFLOW_LISTEN_CMD` and run `handler_graph` once per event.
async fn listen_loop(
    handler_graph: &str,
    graphs_dir: &Path,
    runs_dir: &Path,
    state_dir: &Path,
    base_inputs: BTreeMap<String, Value>,
    no_notify: bool,
) -> Result<(), String> {
    let cmd = listen_command()?;
    eprintln!("[listen] starting: {cmd}");
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to start listener: {e}"))?;

    let stdout = child.stdout.take().ok_or("listener produced no stdout")?;
    let mut lines = BufReader::new(stdout).lines();

    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        let Some(inputs) = event_to_inputs(&line, &base_inputs) else {
            continue;
        };
        eprintln!("[listen] event -> {handler_graph}");
        match run(
            handler_graph,
            graphs_dir,
            runs_dir,
            state_dir,
            inputs,
            no_notify,
        )
        .await
        {
            Ok(true) => {}
            Ok(false) => eprintln!("[listen] handler '{handler_graph}' reported failure"),
            Err(e) => eprintln!("[listen] dispatch error: {e}"),
        }
    }

    let _ = child.wait().await;
    Ok(())
}

/// Load `<graphs-dir>/config.json` (a flat object of constants) as low-precedence
/// `{ctx.*}` defaults. Missing/invalid file → empty map.
fn load_config(graphs_dir: &Path) -> BTreeMap<String, Value> {
    let path = graphs_dir.join(CONFIG_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Object(map)) => {
            map.into_iter().map(|(k, v)| (k, Value::from(v))).collect()
        }
        _ => {
            eprintln!("warning: {} is not a JSON object; ignoring", path.display());
            BTreeMap::new()
        }
    }
}

/// Load a graph's cross-run state (`<state-dir>/<graph>.json`, a flat object).
fn load_state(state_dir: &Path, graph_name: &str) -> BTreeMap<String, Value> {
    let path = state_dir.join(format!("{graph_name}.json"));
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Object(map)) => {
            map.into_iter().map(|(k, v)| (k, Value::from(v))).collect()
        }
        _ => BTreeMap::new(),
    }
}

/// Persist node outputs prefixed `save_` (prefix stripped) into the graph's
/// cross-run state, merging over any existing keys.
fn save_state(state_dir: &Path, graph_name: &str, result: &ExecutionResult) -> Result<(), String> {
    let mut to_save: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for outputs in result.node_outputs.values() {
        for (k, v) in outputs {
            if let Some(key) = k.strip_prefix(STATE_SAVE_PREFIX) {
                to_save.insert(key.to_string(), serde_json::Value::from(v));
            }
        }
    }
    if to_save.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(state_dir).map_err(|e| e.to_string())?;
    let path = state_dir.join(format!("{graph_name}.json"));
    // Merge over existing state.
    let mut existing: serde_json::Map<String, serde_json::Value> =
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        {
            Some(serde_json::Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
    for (k, v) in to_save {
        existing.insert(k, v);
    }
    let json = serde_json::to_string_pretty(&serde_json::Value::Object(existing))
        .map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    eprintln!("state saved: {}", path.display());
    Ok(())
}

/// Print available graphs (name + description) in `graphs_dir`.
fn list_graphs(graphs_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(graphs_dir) else {
        eprintln!("no graphs dir at {}", graphs_dir.display());
        return;
    };
    let mut rows: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mmd") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let desc = std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| load_mermaid(&c).ok())
            .and_then(|g| g.metadata().description.clone())
            .unwrap_or_default();
        rows.push((name, desc));
    }
    rows.sort();
    for (name, desc) in rows {
        if desc.is_empty() {
            println!("{name}");
        } else {
            println!("{name}  —  {desc}");
        }
    }
}

/// Collect the `{ctx.KEY}` keys referenced anywhere in the raw graph text, so the
/// runner can fail fast on missing inputs instead of erroring mid-render.
fn referenced_ctx_keys(content: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    let mut rest = content;
    while let Some(pos) = rest.find(CTX_MARKER) {
        let after = &rest[pos + CTX_MARKER.len()..];
        let key: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let consumed = key.len();
        if !key.is_empty() {
            keys.insert(key);
        }
        rest = &after[consumed..];
    }
    keys
}

/// Returns Ok(true) on success, Ok(false) when a node failed.
async fn run(
    graph_ref: &str,
    graphs_dir: &Path,
    runs_dir: &Path,
    state_dir: &Path,
    cli_inputs: BTreeMap<String, Value>,
    no_notify: bool,
) -> Result<bool, String> {
    let path = resolve_graph_path(graph_ref, graphs_dir)?;
    let graph_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(graph_ref)
        .to_string();

    // Precedence (low → high): config-file defaults < cross-run state < --input.
    let mut inputs = load_config(graphs_dir);
    inputs.extend(load_state(state_dir, &graph_name));
    inputs.extend(cli_inputs);

    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    // Fail fast on missing inputs, before any node runs.
    let missing: Vec<String> = referenced_ctx_keys(&content)
        .into_iter()
        .filter(|k| !inputs.contains_key(k))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing required input(s): {} (pass via --input k=v or {CONFIG_FILE})",
            missing.join(", ")
        ));
    }

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
    let handlers = build_handlers(adapter.clone(), inputs.clone(), graphs_dir);
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
        // Persist any `save_*` outputs to cross-run state (success only).
        if let Err(e) = save_state(state_dir, &graph_name, &result) {
            eprintln!("warning: could not write state: {e}");
        }
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

/// Build the handler registry: the provider-neutral psflow defaults wired to a
/// resolver that exposes runtime inputs as `{ctx.key}`, plus `llm_call` — then
/// any optional integrations (see `register_integrations`).
fn build_handlers(
    adapter: Arc<ClaudeCliAdapter>,
    inputs: BTreeMap<String, Value>,
    graphs_dir: &Path,
) -> psflow::execute::HandlerRegistry {
    let engine = Arc::new(ScriptEngine::with_defaults());

    // A context whose blackboard carries the runtime inputs, so context-aware
    // handlers (`llm_call` prompts, `rhai` scripts, subgraphs) can read them as
    // `{ctx.key}` / `ctx_get(ctx, "key")`.
    let ctx = Arc::new(ExecutionContext::new());
    {
        let mut bb = ctx.blackboard();
        for (k, v) in &inputs {
            bb.set(k.clone(), v.clone(), BlackboardScope::Global);
        }
    }

    let resolver: Arc<dyn TemplateResolver> = Arc::new(RuntimeInputResolver {
        inputs,
        inner: PromptTemplateResolver,
    });
    let mut reg = NodeRegistry::with_defaults_and_resolver(engine.clone(), resolver.clone());
    reg.register(
        "llm_call",
        Arc::new(LlmCallHandler::with_context(adapter, ctx.clone())),
    );
    // Override the default stateless `rhai` with one that sees the inputs as `ctx`.
    let rhai = RhaiHandler::new(engine);
    rhai.set_context(ctx.clone());
    reg.register("rhai", Arc::new(rhai));

    register_integrations(&mut reg, &resolver);

    // Composition: every `.mmd` in graphs_dir is a callable subgraph, so graphs
    // can invoke each other (`subgraph_invoke`) and fan out over a runtime list
    // (`map`). Both need the final registry (set after into_handler_registry).
    let library = Arc::new(load_graph_library(graphs_dir));
    let (subgraph, sub_slot) = SubgraphInvocationHandler::new(library.clone());
    reg.register(
        "subgraph_invoke",
        Arc::new(subgraph.with_context(ctx.clone())),
    );
    let (map_handler, map_slot) = MapHandler::new(library);
    reg.register("map", Arc::new(map_handler.with_context(ctx)));

    let handlers = reg.into_handler_registry();
    sub_slot.set(handlers.clone());
    map_slot.set(handlers.clone());
    handlers
}

/// Load every `<graphs-dir>/*.mmd` as a named subgraph (name = file stem) so
/// `subgraph_invoke`/`map` can reference them. Unparseable files are skipped.
fn load_graph_library(graphs_dir: &Path) -> GraphLibrary {
    let mut lib = GraphLibrary::new();
    let Ok(entries) = std::fs::read_dir(graphs_dir) else {
        return lib;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("mmd") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        match load_mermaid(&content) {
            Ok(g) => {
                lib.register(name, g);
            }
            Err(_) => eprintln!("warning: skipped subgraph '{name}' (parse error)"),
        }
    }
    lib
}

/// Optional third-party integrations, kept out of the provider-neutral core so
/// they can be dropped cleanly. To remove an integration: delete its block here,
/// its handler module under `src/handlers/`, and any provider-specific graphs.
fn register_integrations(reg: &mut NodeRegistry, resolver: &Arc<dyn TemplateResolver>) {
    // --- Composio (remove this block + src/handlers/composio.rs to drop it) ---
    reg.register(
        "composio",
        Arc::new(psflow::handlers::composio::ComposioHandler::new(
            resolver.clone(),
        )),
    );
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

/// Tool handlers may emit a `log_id` output (e.g. for provider-side forensics);
/// collect any non-empty ones, keyed by node.
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
        "tool_log_ids": collect_log_ids(result),
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
                let handlers =
                    build_handlers(Arc::new(ClaudeCliAdapter::new()), hook_inputs, graphs_dir);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_to_inputs_skips_non_json() {
        let base = BTreeMap::new();
        assert!(event_to_inputs("banner line", &base).is_none());
        assert!(event_to_inputs("   ", &base).is_none());
    }

    #[test]
    fn event_to_inputs_adds_event_over_base() {
        let mut base = BTreeMap::new();
        base.insert("k".to_string(), Value::String("v".to_string()));
        let out = event_to_inputs("  {\"id\":\"e1\"}  ", &base).unwrap();
        assert_eq!(out.get("k"), Some(&Value::String("v".to_string())));
        match out.get("event") {
            Some(Value::String(s)) => assert!(s.contains("e1")),
            other => panic!("expected event string, got {other:?}"),
        }
    }

    #[test]
    fn referenced_ctx_keys_extracts_names() {
        let keys = referenced_ctx_keys("a {ctx.sheet_id} b {ctx.query} c {inputs.x}");
        assert!(keys.contains("sheet_id"));
        assert!(keys.contains("query"));
        assert!(!keys.contains("x"));
    }
}
