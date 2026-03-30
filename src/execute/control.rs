use crate::error::NodeError;
use crate::execute::blackboard::{Blackboard, BlackboardScope};
use crate::execute::context::ExecutionContext;
use crate::execute::event::ExecutionEvent;
use crate::execute::lifecycle::NodeState;
use crate::execute::{ExecutionError, HandlerRegistry, NodeHandler, Outputs};
use crate::graph::node::NodeId;
use crate::graph::Graph;
use futures::future::select_all;
use std::sync::Arc;
use std::time::Duration;

/// Default maximum iterations for `LoopConfig::While` to prevent runaway loops.
pub const DEFAULT_MAX_LOOP_ITERATIONS: usize = 10_000;

/// Result of evaluating a guard expression.
#[derive(Debug, Clone, PartialEq)]
pub enum GuardResult {
    /// Guard evaluated to a boolean.
    Bool(bool),
    /// Guard evaluated to a string label for edge matching.
    Label(String),
}

impl GuardResult {
    /// The edge label this guard result matches.
    /// `true` maps to `"yes"`, `false` maps to `"no"`, labels pass through.
    pub fn edge_label(&self) -> &str {
        match self {
            GuardResult::Bool(true) => "yes",
            GuardResult::Bool(false) => "no",
            GuardResult::Label(s) => s,
        }
    }
}

/// Evaluate a guard expression against node inputs and the blackboard.
///
/// Uses the Rhai scripting engine for expression evaluation, supporting the full
/// range of operators (`>`, `<`, `>=`, `<=`, `&&`, `||`, `!`), arithmetic,
/// string methods (`len()`, `contains()`, etc.), and nested property access.
///
/// Two variables are injected into the Rhai scope:
/// - `inputs` — a Map built from node output values
/// - `ctx` — a Map built from the blackboard (global scope)
///
/// The result type determines `GuardResult`:
/// - Boolean → `GuardResult::Bool`
/// - String  → `GuardResult::Label`
///
/// **Backwards compatibility:** If Rhai evaluation fails (e.g. unquoted string
/// literals like `inputs.status == active`), falls back to the legacy evaluator.
pub fn evaluate_guard(
    expr: &str,
    inputs: &Outputs,
    blackboard: &Blackboard,
) -> Result<GuardResult, NodeError> {
    let expr = expr.trim();

    // Try Rhai evaluation first
    match eval_guard_rhai(expr, inputs, blackboard) {
        Ok(result) => return Ok(result),
        Err(_) => {
            // Fall back to legacy evaluator for backwards compatibility
        }
    }

    eval_guard_legacy(expr, inputs, blackboard)
}

/// Rhai-based guard evaluation.
fn eval_guard_rhai(
    expr: &str,
    inputs: &Outputs,
    blackboard: &Blackboard,
) -> Result<GuardResult, NodeError> {
    use crate::scripting::bridge::{outputs_to_rhai_map, value_to_dynamic};
    use crate::scripting::engine::ScriptEngine;
    use rhai::Dynamic;
    use std::sync::OnceLock;
    use tokio_util::sync::CancellationToken;

    // Cache a single ScriptEngine for all guard evaluations to avoid
    // constructing two Rhai Engine instances on every branch/loop check.
    static GUARD_ENGINE: OnceLock<ScriptEngine> = OnceLock::new();
    let engine = GUARD_ENGINE.get_or_init(ScriptEngine::with_defaults);
    let cancel = CancellationToken::new();
    let mut scope = rhai::Scope::new();

    // Build `inputs` Map from node outputs
    let inputs_map = outputs_to_rhai_map(inputs);
    scope.push_dynamic("inputs", Dynamic::from_map(inputs_map));

    // Build `ctx` Map from blackboard global scope
    let mut ctx_map = rhai::Map::new();
    for (key, value) in blackboard.global() {
        ctx_map.insert(key.clone().into(), value_to_dynamic(value));
    }
    scope.push_dynamic("ctx", Dynamic::from_map(ctx_map));

    let result = engine
        .eval_expression(expr, &mut scope, &cancel)
        .map_err(|e| NodeError::Failed {
            source_message: None,
            message: format!("guard expression error: {e}"),
            recoverable: false,
        })?;

    if result.is_bool() {
        Ok(GuardResult::Bool(result.as_bool().unwrap()))
    } else if result.is_string() {
        Ok(GuardResult::Label(result.into_string().unwrap()))
    } else if result.is_int() {
        // Treat nonzero as truthy
        Ok(GuardResult::Bool(result.as_int().unwrap() != 0))
    } else {
        Ok(GuardResult::Bool(!result.is_unit()))
    }
}

/// Legacy hand-rolled guard evaluator for backwards compatibility.
///
/// Handles forms that Rhai can't parse (e.g. unquoted string literals):
/// - `"true"` / `"false"` — boolean literal
/// - `"inputs.key"` — truthy check
/// - `"inputs.key == value"` — equality (unquoted RHS)
/// - `"inputs.key != value"` — inequality
/// - `"ctx.key"` / `"ctx.key == value"` — blackboard lookup
fn eval_guard_legacy(
    expr: &str,
    inputs: &Outputs,
    blackboard: &Blackboard,
) -> Result<GuardResult, NodeError> {
    // Boolean literals
    if expr == "true" {
        return Ok(GuardResult::Bool(true));
    }
    if expr == "false" {
        return Ok(GuardResult::Bool(false));
    }

    // Comparison operators
    for (op, negate) in [("!=", true), ("==", false)] {
        if let Some((lhs, rhs)) = expr.split_once(op) {
            let lhs = lhs.trim();
            let rhs = rhs.trim();
            let val = resolve_ref(lhs, inputs, blackboard)?;
            let expected = parse_literal(rhs);
            let eq = value_equals_str(&val, &expected);
            return Ok(GuardResult::Bool(if negate { !eq } else { eq }));
        }
    }

    // Bare reference — truthy check
    let val = resolve_ref(expr, inputs, blackboard)?;
    Ok(GuardResult::Bool(is_truthy(&val)))
}

/// Resolve a dotted reference like `inputs.key` or `ctx.key` to a string representation.
fn resolve_ref(
    reference: &str,
    inputs: &Outputs,
    blackboard: &Blackboard,
) -> Result<String, NodeError> {
    if let Some(key) = reference.strip_prefix("inputs.") {
        inputs
            .get(key)
            .map(value_to_string)
            .ok_or_else(|| NodeError::Failed {
                source_message: None,
                message: format!("guard: input '{key}' not found"),
                recoverable: false,
            })
    } else if let Some(key) = reference.strip_prefix("ctx.") {
        blackboard
            .get(key, &BlackboardScope::Global)
            .map(value_to_string)
            .ok_or_else(|| NodeError::Failed {
                source_message: None,
                message: format!("guard: context key '{key}' not found"),
                recoverable: false,
            })
    } else {
        // Treat as a literal string
        Ok(reference.to_string())
    }
}

fn value_to_string(v: &crate::graph::types::Value) -> String {
    use crate::graph::types::Value;
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::I64(n) => n.to_string(),
        Value::F32(n) => n.to_string(),
        Value::Null => "null".to_string(),
        other => format!("{other:?}"),
    }
}

fn parse_literal(s: &str) -> String {
    // Strip surrounding quotes if present (must be at least 2 chars for open+close)
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"'))
            || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn value_equals_str(val: &str, expected: &str) -> bool {
    val == expected
}

fn is_truthy(val: &str) -> bool {
    !val.is_empty() && val != "false" && val != "0" && val != "null"
}

// ---------------------------------------------------------------------------
// LLM oracle evaluations
// ---------------------------------------------------------------------------

/// Evaluate a branch guard via LLM adapter.
///
/// Config schema (`config.guard_llm`):
/// - `prompt`: Template string with `{inputs.*}` / `{ctx.*}` placeholders
/// - `output`: Expected output type: `"bool"` (default) or `"label"`
/// - `fallback`: Deterministic guard expression used if adapter fails
///
/// Returns the edge label string ("yes"/"no" for bool, or the label text).
pub async fn evaluate_guard_llm(
    llm_config: &serde_json::Value,
    inputs: &Outputs,
    ctx: &ExecutionContext,
    adapter: &dyn crate::adapter::AiAdapter,
) -> Result<String, NodeError> {
    let prompt_template = llm_config
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| NodeError::Failed {
            source_message: None,
            message: "guard_llm: missing 'prompt' field".into(),
            recoverable: false,
        })?;

    let output_type = llm_config
        .get("output")
        .and_then(|v| v.as_str())
        .unwrap_or("bool");

    let prompt = render_llm_prompt(prompt_template, inputs, ctx);

    let request = crate::adapter::AiRequest::new(prompt);
    let response = adapter.complete(request).await?;
    let text = response.text.trim().to_lowercase();

    match output_type {
        "bool" => {
            let is_true = text == "true" || text == "yes" || text == "1";
            Ok(if is_true { "yes" } else { "no" }.to_string())
        }
        "label" => Ok(response.text.trim().to_string()),
        _ => Ok(response.text.trim().to_string()),
    }
}

/// Evaluate a race winner via LLM adapter's judge capability.
///
/// Takes candidate outputs (as formatted strings) and criteria,
/// returns the index of the winning candidate.
pub async fn evaluate_race_criterion_llm(
    criteria: &str,
    candidate_outputs: &[String],
    adapter: &dyn crate::adapter::AiAdapter,
) -> Result<usize, NodeError> {
    adapter
        .judge(candidate_outputs, criteria)
        .await
}

/// Evaluate a loop continuation condition via LLM adapter.
///
/// Config schema (`exec.while_llm`):
/// - `prompt`: Template string with `{ctx.*}` placeholders
/// - `fallback`: Deterministic guard expression used if adapter fails
///
/// Returns true to continue the loop, false to stop.
pub async fn evaluate_loop_condition_llm(
    llm_config: &serde_json::Value,
    ctx: &ExecutionContext,
    adapter: &dyn crate::adapter::AiAdapter,
) -> Result<bool, NodeError> {
    let prompt_template = llm_config
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| NodeError::Failed {
            source_message: None,
            message: "while_llm: missing 'prompt' field".into(),
            recoverable: false,
        })?;

    let prompt = render_llm_prompt(prompt_template, &Outputs::new(), ctx);

    let request = crate::adapter::AiRequest::new(prompt);
    let response = adapter.complete(request).await?;
    let text = response.text.trim().to_lowercase();

    Ok(text == "true" || text == "yes" || text == "continue" || text == "1")
}

/// Simple prompt template rendering with `{inputs.*}` and `{ctx.*}` placeholders.
fn render_llm_prompt(template: &str, inputs: &Outputs, ctx: &ExecutionContext) -> String {
    let mut result = template.to_string();

    // Replace {inputs.key} placeholders
    for (key, value) in inputs {
        let placeholder = format!("{{inputs.{key}}}");
        let replacement = value_to_string(value);
        result = result.replace(&placeholder, &replacement);
    }

    // Replace {ctx.key} placeholders from blackboard
    let bb = ctx.blackboard();
    // Find all {ctx.*} patterns and resolve them
    while let Some(start) = result.find("{ctx.") {
        let rest = &result[start + 5..];
        if let Some(end) = rest.find('}') {
            let key = &rest[..end];
            let replacement = bb
                .get(key, &BlackboardScope::Global)
                .map(value_to_string)
                .unwrap_or_default();
            result = format!("{}{}{}", &result[..start], replacement, &rest[end + 1..]);
        } else {
            break;
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Subgraph execution strategies
// ---------------------------------------------------------------------------

/// Configuration for loop execution.
#[derive(Debug, Clone)]
pub enum LoopConfig {
    /// Repeat a fixed number of times.
    Repeat(usize),
    /// Repeat while a guard expression evaluates to true (checked before each iteration).
    /// `max_iterations` prevents runaway loops (defaults to `DEFAULT_MAX_LOOP_ITERATIONS`).
    While {
        guard: String,
        max_iterations: usize,
    },
    /// Repeat while an LLM adapter says to continue. Falls back to deterministic
    /// guard (`fallback` field) if adapter is unavailable or errors.
    WhileLlm {
        llm_config: serde_json::Value,
        max_iterations: usize,
    },
}

/// Execute subgraph nodes in declaration order (sequence).
///
/// Each node runs only after the previous one completes.
/// If `fail_fast` is true (default), a failure stops the sequence immediately.
pub async fn execute_sequence(
    node_ids: &[NodeId],
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
    fail_fast: bool,
) -> Result<(), ExecutionError> {
    for node_id in node_ids {
        if ctx.is_cancelled() {
            cancel_remaining(node_ids, ctx);
            break;
        }

        let state = ctx.get_state(&node_id.0);
        if state.is_terminal() {
            continue;
        }

        let result = execute_single_node(node_id, graph, handlers, ctx, passthrough).await?;
        if result.is_err() && fail_fast {
            cancel_downstream_of(node_id, node_ids, graph, ctx);
            return Ok(());
        }
    }
    Ok(())
}

/// Execute subgraph nodes concurrently.
///
/// All nodes launch simultaneously unless `max_concurrent` is set,
/// in which case at most that many run at once (via a local semaphore).
/// Also respects the global concurrency limit from `ExecutionContext`.
pub async fn execute_parallel(
    node_ids: &[NodeId],
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
    max_concurrent: Option<usize>,
) -> Result<(), ExecutionError> {
    let local_sem = max_concurrent.map(super::concurrency::subgraph_semaphore);
    let mut handles = Vec::new();

    for node_id in node_ids {
        if ctx.is_cancelled() || ctx.get_state(&node_id.0).is_terminal() {
            continue;
        }

        // Acquire permits BEFORE spawning — blocks when at limit.
        // Permits are moved into wrapper tasks so they're held until completion.
        let local_permit = if let Some(ref sem) = local_sem {
            Some(
                sem.clone()
                    .acquire_owned()
                    .await
                    .expect("local semaphore closed"),
            )
        } else {
            None
        };
        let global_permit = ctx.concurrency().acquire().await;

        let inner_handle =
            spawn_node_task(node_id.clone(), graph, handlers, ctx, passthrough.clone())?;

        // Wrap the task to hold permits until the inner task completes
        let handle = tokio::spawn(async move {
            let _local = local_permit;
            let _global = global_permit;
            inner_handle.await.unwrap_or_else(|e| {
                ("panic".to_string(), Err(NodeError::Failed {
                    source_message: None,
                    message: format!("task panic: {e}"),
                    recoverable: false,
                }))
            })
        });
        handles.push(handle);
    }

    collect_results(handles, graph, ctx).await
}

/// Execute subgraph nodes as a race: first to complete successfully wins,
/// remaining siblings are aborted and marked cancelled.
///
/// Uses `futures::future::select_all` for true concurrent racing —
/// the winner is determined by actual completion time, not spawn order.
///
/// If an `adapter` is provided and any node has `exec.criterion_llm`,
/// all candidates run to completion and the adapter picks the winner.
/// Returns the winning node's ID, or `None` if all failed.
pub async fn execute_race(
    node_ids: &[NodeId],
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
) -> Result<Option<NodeId>, ExecutionError> {
    execute_race_with_adapter(node_ids, graph, handlers, ctx, passthrough, None).await
}

/// Execute a race with optional LLM criterion for winner selection.
pub async fn execute_race_with_adapter(
    node_ids: &[NodeId],
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
    adapter: Option<&dyn crate::adapter::AiAdapter>,
) -> Result<Option<NodeId>, ExecutionError> {
    if node_ids.is_empty() {
        return Ok(None);
    }

    // Check if any node has criterion_llm — if so, run all candidates then judge
    let criterion_llm = node_ids.iter().find_map(|id| {
        graph.node(id).and_then(|n| n.exec.get("criterion_llm").cloned())
    });

    if let (Some(criterion_config), Some(adapter)) = (&criterion_llm, adapter) {
        return execute_race_with_criterion(
            node_ids,
            graph,
            handlers,
            ctx,
            passthrough,
            criterion_config,
            adapter,
        )
        .await;
    }

    let mut task_ids: Vec<NodeId> = Vec::new();
    let mut futures = Vec::new();

    for node_id in node_ids {
        if ctx.is_cancelled() || ctx.get_state(&node_id.0).is_terminal() {
            continue;
        }

        let handle =
            spawn_node_task(node_id.clone(), graph, handlers, ctx, passthrough.clone())?;
        task_ids.push(node_id.clone());
        futures.push(handle);
    }

    if futures.is_empty() {
        return Ok(None);
    }

    let mut winner = None;
    let mut remaining = futures;

    // Race loop: poll all futures, process the first to complete, repeat until
    // we have a winner or all tasks have finished.
    while !remaining.is_empty() {
        let (result, _index, rest) = select_all(remaining).await;

        let (id_str, outcome) = result
            .map_err(|e| ExecutionError::ValidationFailed(format!("task panic: {e}")))?;

        match outcome {
            Ok(outputs) => {
                ctx.store_outputs(&id_str, outputs.clone());
                ctx.emit(ExecutionEvent::NodeCompleted {
                    node_id: id_str.clone(),
                    outputs,
                });
                let _ = ctx.set_state(&id_str, NodeState::Completed);

                if winner.is_none() {
                    winner = Some(NodeId::new(&id_str));

                    // Abort remaining tasks and mark siblings cancelled
                    for handle in &rest {
                        handle.abort();
                    }
                    for other in node_ids {
                        if other.0 != id_str && !ctx.get_state(&other.0).is_terminal() {
                            let _ = ctx.set_state(&other.0, NodeState::Cancelled);
                        }
                    }
                    break;
                }
            }
            Err(NodeError::Cancelled { .. }) => {
                let _ = ctx.set_state(&id_str, NodeState::Cancelled);
            }
            Err(ref error) => {
                ctx.emit(ExecutionEvent::NodeFailed {
                    node_id: id_str.clone(),
                    error: error.clone(),
                });
                let _ = ctx.set_state(&id_str, NodeState::Failed);
            }
        }

        remaining = rest;
    }

    Ok(winner)
}

/// Execute subgraph nodes in a loop.
pub async fn execute_loop(
    node_ids: &[NodeId],
    config: &LoopConfig,
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
) -> Result<(), ExecutionError> {
    execute_loop_with_adapter(node_ids, config, graph, handlers, ctx, passthrough, None).await
}

/// Execute a loop with optional LLM adapter for WhileLlm conditions.
pub async fn execute_loop_with_adapter(
    node_ids: &[NodeId],
    config: &LoopConfig,
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
    adapter: Option<&dyn crate::adapter::AiAdapter>,
) -> Result<(), ExecutionError> {
    match config {
        LoopConfig::Repeat(count) => {
            for _ in 0..*count {
                if ctx.is_cancelled() {
                    break;
                }
                reset_node_states(node_ids, ctx);
                execute_sequence(node_ids, graph, handlers, ctx, passthrough, true).await?;
            }
        }
        LoopConfig::While {
            guard: guard_expr,
            max_iterations,
        } => {
            for _ in 0..*max_iterations {
                if ctx.is_cancelled() {
                    break;
                }

                let should_continue = {
                    let bb = ctx.blackboard();
                    evaluate_guard(guard_expr, &Outputs::new(), &bb)
                        .unwrap_or(GuardResult::Bool(false))
                };

                if should_continue != GuardResult::Bool(true) {
                    break;
                }

                reset_node_states(node_ids, ctx);
                execute_sequence(node_ids, graph, handlers, ctx, passthrough, true).await?;
            }
        }
        LoopConfig::WhileLlm {
            llm_config,
            max_iterations,
        } => {
            for _ in 0..*max_iterations {
                if ctx.is_cancelled() {
                    break;
                }

                // Try LLM condition first
                let should_continue = if let Some(adapter) = adapter {
                    match evaluate_loop_condition_llm(llm_config, ctx, adapter).await {
                        Ok(result) => result,
                        Err(_) => {
                            // Fallback to deterministic guard
                            if let Some(fallback) = llm_config.get("fallback").and_then(|v| v.as_str()) {
                                let bb = ctx.blackboard();
                                let result = evaluate_guard(fallback, &Outputs::new(), &bb);
                                drop(bb);
                                result.unwrap_or(GuardResult::Bool(false)) == GuardResult::Bool(true)
                            } else {
                                false // No fallback, stop the loop
                            }
                        }
                    }
                } else {
                    // No adapter — use fallback
                    if let Some(fallback) = llm_config.get("fallback").and_then(|v| v.as_str()) {
                        let bb = ctx.blackboard();
                        let result = evaluate_guard(fallback, &Outputs::new(), &bb);
                        drop(bb);
                        result.unwrap_or(GuardResult::Bool(false)) == GuardResult::Bool(true)
                    } else {
                        false
                    }
                };

                if !should_continue {
                    break;
                }

                reset_node_states(node_ids, ctx);
                execute_sequence(node_ids, graph, handlers, ctx, passthrough, true).await?;
            }
        }
    }
    Ok(())
}

/// Race variant: run all candidates to completion, then ask the adapter to pick the winner.
async fn execute_race_with_criterion(
    node_ids: &[NodeId],
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
    criterion_config: &serde_json::Value,
    adapter: &dyn crate::adapter::AiAdapter,
) -> Result<Option<NodeId>, ExecutionError> {
    let criteria = criterion_config
        .get("criteria")
        .and_then(|v| v.as_str())
        .unwrap_or("Pick the best candidate.");

    // Run all candidates concurrently (like parallel, not first-wins)
    execute_parallel(node_ids, graph, handlers, ctx, passthrough, None).await?;

    // Collect completed candidates and their outputs
    let mut candidates: Vec<(NodeId, String)> = Vec::new();
    for node_id in node_ids {
        if ctx.get_state(&node_id.0) == NodeState::Completed {
            let output_str = ctx
                .get_outputs(&node_id.0)
                .map(|o| format!("{o:?}"))
                .unwrap_or_default();
            candidates.push((node_id.clone(), output_str));
        }
    }

    if candidates.is_empty() {
        return Ok(None);
    }

    if candidates.len() == 1 {
        return Ok(Some(candidates[0].0.clone()));
    }

    // Ask the adapter to judge
    let candidate_texts: Vec<String> = candidates.iter().map(|(_, s)| s.clone()).collect();
    let winner_idx = evaluate_race_criterion_llm(criteria, &candidate_texts, adapter)
        .await
        .unwrap_or(0); // fallback to first candidate on error

    let winner_idx = winner_idx.min(candidates.len() - 1);
    Ok(Some(candidates[winner_idx].0.clone()))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Execute a single node: transition states, call handler, store results.
async fn execute_single_node(
    node_id: &NodeId,
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
) -> Result<Result<Outputs, NodeError>, ExecutionError> {
    let node = graph.node(node_id).ok_or_else(|| {
        ExecutionError::ValidationFailed(format!("node '{}' not found", node_id))
    })?;

    let handler: Arc<dyn NodeHandler> = node
        .handler
        .as_ref()
        .and_then(|name| handlers.get(name))
        .cloned()
        .unwrap_or_else(|| passthrough.clone());

    let inputs = super::topological::collect_inputs(graph, node_id, ctx);

    // Per-node timeout
    let timeout_dur = node
        .exec
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis);

    ctx.set_state(&node_id.0, NodeState::Pending)
        .map_err(|e| ExecutionError::ValidationFailed(e.to_string()))?;
    ctx.set_state(&node_id.0, NodeState::Running)
        .map_err(|e| ExecutionError::ValidationFailed(e.to_string()))?;

    let cancel = ctx.cancel_token().clone();
    let retry_config = super::retry::RetryConfig::from_exec(&node.exec);

    // Core execution: retry wraps handler calls, node timeout wraps the entire sequence
    let execute_fn = async {
        if let Some(ref rc) = retry_config {
            super::retry::execute_with_retry_ctx(&handler, node, inputs, cancel.clone(), rc, Some(ctx)).await
        } else {
            handler.execute(node, inputs, cancel.clone()).await
        }
    };

    let result = if let Some(timeout) = timeout_dur {
        match tokio::time::timeout(timeout, execute_fn).await {
            Ok(r) => r,
            Err(_) => Err(NodeError::Timeout {
                elapsed_ms: timeout.as_millis() as u64,
                limit_ms: timeout.as_millis() as u64,
            }),
        }
    } else {
        execute_fn.await
    };

    match &result {
        Ok(outputs) => {
            ctx.store_outputs(&node_id.0, outputs.clone());
            ctx.emit(ExecutionEvent::NodeCompleted {
                node_id: node_id.0.clone(),
                outputs: outputs.clone(),
            });
            let _ = ctx.set_state(&node_id.0, NodeState::Completed);
        }
        Err(NodeError::Cancelled { .. }) => {
            let _ = ctx.set_state(&node_id.0, NodeState::Cancelled);
        }
        Err(error) => {
            ctx.emit(ExecutionEvent::NodeFailed {
                node_id: node_id.0.clone(),
                error: error.clone(),
            });
            let _ = ctx.set_state(&node_id.0, NodeState::Failed);
        }
    }

    Ok(result)
}

type NodeTaskHandle = tokio::task::JoinHandle<(String, Result<Outputs, NodeError>)>;

/// Spawn a node's execution as a tokio task.
fn spawn_node_task(
    node_id: NodeId,
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: Arc<dyn NodeHandler>,
) -> Result<NodeTaskHandle, ExecutionError> {
    let node = graph.node(&node_id).ok_or_else(|| {
        ExecutionError::ValidationFailed(format!("node '{}' not found", node_id))
    })?;

    let handler: Arc<dyn NodeHandler> = node
        .handler
        .as_ref()
        .and_then(|name| handlers.get(name))
        .cloned()
        .unwrap_or(passthrough);

    let inputs = super::topological::collect_inputs(graph, &node_id, ctx);
    let node_clone = node.clone();
    let node_id_str = node_id.0.clone();
    let cancel = ctx.cancel_token().clone();
    let ctx_clone = ctx.clone();

    let timeout_dur = node
        .exec
        .get("timeout_ms")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis);

    let retry_config = super::retry::RetryConfig::from_exec(&node.exec);

    ctx.set_state(&node_id_str, NodeState::Pending)
        .map_err(|e| ExecutionError::ValidationFailed(e.to_string()))?;

    Ok(tokio::spawn(async move {
        if cancel.is_cancelled() {
            return (
                node_id_str,
                Err(NodeError::Cancelled {
                    reason: "execution cancelled".into(),
                }),
            );
        }

        if let Err(e) = ctx_clone.set_state(&node_id_str, NodeState::Running) {
            return (node_id_str, Err(e));
        }

        let execute_fn = async {
            if let Some(ref rc) = retry_config {
                super::retry::execute_with_retry_ctx(
                    &handler,
                    &node_clone,
                    inputs,
                    cancel.clone(),
                    rc,
                    Some(&ctx_clone),
                )
                .await
            } else {
                handler.execute(&node_clone, inputs, cancel.clone()).await
            }
        };

        let result = if let Some(timeout) = timeout_dur {
            match tokio::time::timeout(timeout, execute_fn).await {
                Ok(r) => r,
                Err(_) => Err(NodeError::Timeout {
                    elapsed_ms: timeout.as_millis() as u64,
                    limit_ms: timeout.as_millis() as u64,
                }),
            }
        } else {
            execute_fn.await
        };

        (node_id_str, result)
    }))
}

/// Await all spawned task handles and process their results.
async fn collect_results(
    handles: Vec<tokio::task::JoinHandle<(String, Result<Outputs, NodeError>)>>,
    graph: &Graph,
    ctx: &Arc<ExecutionContext>,
) -> Result<(), ExecutionError> {
    for handle in handles {
        let (node_id, outcome) = handle
            .await
            .map_err(|e| ExecutionError::ValidationFailed(format!("task panic: {e}")))?;

        match outcome {
            Ok(outputs) => {
                ctx.store_outputs(&node_id, outputs.clone());
                ctx.emit(ExecutionEvent::NodeCompleted {
                    node_id: node_id.clone(),
                    outputs,
                });
                let _ = ctx.set_state(&node_id, NodeState::Completed);
            }
            Err(NodeError::Cancelled { .. }) => {
                let _ = ctx.set_state(&node_id, NodeState::Cancelled);
            }
            Err(ref error) => {
                ctx.emit(ExecutionEvent::NodeFailed {
                    node_id: node_id.clone(),
                    error: error.clone(),
                });
                let _ = ctx.set_state(&node_id, NodeState::Failed);
                // Cancel downstream nodes within the graph
                cancel_downstream_in_graph(&NodeId::new(&node_id), graph, ctx);
            }
        }
    }
    Ok(())
}

/// Cancel all non-terminal nodes in the list.
fn cancel_remaining(node_ids: &[NodeId], ctx: &ExecutionContext) {
    for id in node_ids {
        if !ctx.get_state(&id.0).is_terminal() {
            let _ = ctx.set_state(&id.0, NodeState::Cancelled);
        }
    }
}

/// Cancel nodes that come after the failed node in the sequence.
fn cancel_downstream_of(
    failed: &NodeId,
    sequence: &[NodeId],
    _graph: &Graph,
    ctx: &ExecutionContext,
) {
    let mut past_failed = false;
    for id in sequence {
        if id == failed {
            past_failed = true;
            continue;
        }
        if past_failed && !ctx.get_state(&id.0).is_terminal() {
            let _ = ctx.set_state(&id.0, NodeState::Cancelled);
        }
    }
}

/// Cancel all downstream (transitive successors) of a node in the graph.
fn cancel_downstream_in_graph(failed_id: &NodeId, graph: &Graph, ctx: &ExecutionContext) {
    let mut stack = vec![failed_id.clone()];
    let mut visited = std::collections::HashSet::new();
    visited.insert(failed_id.clone());

    while let Some(current) = stack.pop() {
        for successor in graph.successors(&current) {
            if visited.insert(successor.id.clone()) {
                if !ctx.get_state(&successor.id.0).is_terminal() {
                    let _ = ctx.set_state(&successor.id.0, NodeState::Cancelled);
                }
                stack.push(successor.id.clone());
            }
        }
    }
}

/// Reset node states so they can be re-executed in a loop iteration.
fn reset_node_states(node_ids: &[NodeId], ctx: &ExecutionContext) {
    ctx.reset_states(node_ids.iter().map(|id| id.0.as_str()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::Value;

    // -- Guard evaluation tests --

    #[test]
    fn guard_bool_literals() {
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        assert_eq!(
            evaluate_guard("true", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
        assert_eq!(
            evaluate_guard("false", &inputs, &bb).unwrap(),
            GuardResult::Bool(false)
        );
    }

    #[test]
    fn guard_input_equality() {
        let mut inputs = Outputs::new();
        inputs.insert("status".into(), Value::String("active".into()));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("inputs.status == active", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
        assert_eq!(
            evaluate_guard("inputs.status == inactive", &inputs, &bb).unwrap(),
            GuardResult::Bool(false)
        );
        assert_eq!(
            evaluate_guard("inputs.status != inactive", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_input_truthy() {
        let mut inputs = Outputs::new();
        inputs.insert("flag".into(), Value::Bool(true));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("inputs.flag", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_ctx_lookup() {
        let inputs = Outputs::new();
        let mut bb = Blackboard::new();
        bb.set(
            "mode".into(),
            Value::String("fast".into()),
            BlackboardScope::Global,
        );

        assert_eq!(
            evaluate_guard("ctx.mode == fast", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_missing_input_is_falsy() {
        // With the Rhai evaluator, accessing a missing map key returns () (unit),
        // which is falsy. This is more ergonomic than erroring — callers already
        // use unwrap_or(Bool(false)) for the error case.
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        assert_eq!(
            evaluate_guard("inputs.missing", &inputs, &bb).unwrap(),
            GuardResult::Bool(false)
        );
    }

    #[test]
    fn guard_quoted_literal() {
        let mut inputs = Outputs::new();
        inputs.insert("name".into(), Value::String("hello world".into()));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("inputs.name == \"hello world\"", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_result_edge_labels() {
        assert_eq!(GuardResult::Bool(true).edge_label(), "yes");
        assert_eq!(GuardResult::Bool(false).edge_label(), "no");
        assert_eq!(GuardResult::Label("custom".into()).edge_label(), "custom");
    }

    // -- Rhai guard expression tests --

    #[test]
    fn guard_rhai_comparison_operators() {
        let mut inputs = Outputs::new();
        inputs.insert("score".into(), Value::I64(85));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("inputs.score > 70", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
        assert_eq!(
            evaluate_guard("inputs.score < 90", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
        assert_eq!(
            evaluate_guard("inputs.score >= 85", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
        assert_eq!(
            evaluate_guard("inputs.score <= 85", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
        assert_eq!(
            evaluate_guard("inputs.score > 90", &inputs, &bb).unwrap(),
            GuardResult::Bool(false)
        );
    }

    #[test]
    fn guard_rhai_logical_operators() {
        let mut inputs = Outputs::new();
        inputs.insert("a".into(), Value::Bool(true));
        inputs.insert("b".into(), Value::Bool(false));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("inputs.a && !inputs.b", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
        assert_eq!(
            evaluate_guard("inputs.a || inputs.b", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
        assert_eq!(
            evaluate_guard("inputs.a && inputs.b", &inputs, &bb).unwrap(),
            GuardResult::Bool(false)
        );
    }

    #[test]
    fn guard_rhai_arithmetic() {
        let mut inputs = Outputs::new();
        inputs.insert("x".into(), Value::I64(10));
        inputs.insert("y".into(), Value::I64(3));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("inputs.x + inputs.y > 12", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
        assert_eq!(
            evaluate_guard("inputs.x * inputs.y == 30", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_rhai_string_methods() {
        let mut inputs = Outputs::new();
        inputs.insert("name".into(), Value::String("hello".into()));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("inputs.name.len() > 0", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
        assert_eq!(
            evaluate_guard("inputs.name.contains(\"ell\")", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_rhai_nested_map_access() {
        let mut inner = std::collections::BTreeMap::new();
        inner.insert("score".into(), Value::I64(95));
        let mut inputs = Outputs::new();
        inputs.insert("item".into(), Value::Map(inner));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("inputs.item.score > 90", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_rhai_ctx_and_inputs_combined() {
        let mut inputs = Outputs::new();
        inputs.insert("value".into(), Value::I64(50));
        let mut bb = Blackboard::new();
        bb.set("threshold".into(), Value::I64(40), BlackboardScope::Global);

        assert_eq!(
            evaluate_guard("inputs.value > ctx.threshold", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_rhai_returns_string_label() {
        let mut inputs = Outputs::new();
        inputs.insert("score".into(), Value::I64(85));
        let bb = Blackboard::new();

        let result = evaluate_guard(
            "if inputs.score >= 90 { \"excellent\" } else if inputs.score >= 70 { \"good\" } else { \"poor\" }",
            &inputs,
            &bb,
        ).unwrap();
        assert_eq!(result, GuardResult::Label("good".into()));
    }

    // -- 3.T.13: Backwards compatibility tests --

    #[test]
    fn guard_compat_quoted_equality() {
        // Old-style: inputs.x == "yes" (quoted string RHS)
        let mut inputs = Outputs::new();
        inputs.insert("answer".into(), Value::String("yes".into()));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("inputs.answer == \"yes\"", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_compat_unquoted_equality() {
        // Old-style: inputs.status == active (unquoted RHS — falls back to legacy)
        let mut inputs = Outputs::new();
        inputs.insert("status".into(), Value::String("active".into()));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("inputs.status == active", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_ctx_bare_truthy_via_rhai() {
        // ctx.flag resolves via Rhai (ctx is a Map in scope), not legacy fallback
        let inputs = Outputs::new();
        let mut bb = Blackboard::new();
        bb.set(
            "flag".into(),
            Value::Bool(true),
            BlackboardScope::Global,
        );

        assert_eq!(
            evaluate_guard("ctx.flag", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_rhai_negation() {
        let mut inputs = Outputs::new();
        inputs.insert("active".into(), Value::Bool(false));
        let bb = Blackboard::new();

        assert_eq!(
            evaluate_guard("!inputs.active", &inputs, &bb).unwrap(),
            GuardResult::Bool(true)
        );
    }

    #[test]
    fn guard_rhai_complex_combined() {
        let mut inputs = Outputs::new();
        inputs.insert("age".into(), Value::I64(25));
        inputs.insert("name".into(), Value::String("alice".into()));
        let mut bb = Blackboard::new();
        bb.set("min_age".into(), Value::I64(18), BlackboardScope::Global);

        assert_eq!(
            evaluate_guard(
                "inputs.age >= ctx.min_age && inputs.name.len() > 0",
                &inputs,
                &bb,
            )
            .unwrap(),
            GuardResult::Bool(true)
        );
    }

    // -- LLM oracle tests --

    #[tokio::test]
    async fn guard_llm_returns_yes() {
        use crate::adapter::mock::MockAdapter;
        use std::sync::Arc;

        let adapter = MockAdapter::new().with_default("true");
        let ctx = Arc::new(ExecutionContext::new());

        let config = serde_json::json!({
            "prompt": "Is this good? {inputs.data}",
            "output": "bool"
        });

        let mut inputs = Outputs::new();
        inputs.insert("data".into(), Value::String("test".into()));

        let result = evaluate_guard_llm(&config, &inputs, &ctx, &adapter)
            .await
            .unwrap();
        assert_eq!(result, "yes");
    }

    #[tokio::test]
    async fn guard_llm_returns_no() {
        use crate::adapter::mock::MockAdapter;
        use std::sync::Arc;

        let adapter = MockAdapter::new().with_default("false");
        let ctx = Arc::new(ExecutionContext::new());

        let config = serde_json::json!({
            "prompt": "Is this good?",
            "output": "bool"
        });

        let result = evaluate_guard_llm(&config, &Outputs::new(), &ctx, &adapter)
            .await
            .unwrap();
        assert_eq!(result, "no");
    }

    #[tokio::test]
    async fn guard_llm_label_mode() {
        use crate::adapter::mock::MockAdapter;
        use std::sync::Arc;

        let adapter = MockAdapter::new().with_default("priority_high");
        let ctx = Arc::new(ExecutionContext::new());

        let config = serde_json::json!({
            "prompt": "Classify this item",
            "output": "label"
        });

        let result = evaluate_guard_llm(&config, &Outputs::new(), &ctx, &adapter)
            .await
            .unwrap();
        assert_eq!(result, "priority_high");
    }

    #[tokio::test]
    async fn guard_llm_missing_prompt_errors() {
        use crate::adapter::mock::MockAdapter;
        use std::sync::Arc;

        let adapter = MockAdapter::new();
        let ctx = Arc::new(ExecutionContext::new());

        let config = serde_json::json!({ "output": "bool" });

        let result = evaluate_guard_llm(&config, &Outputs::new(), &ctx, &adapter).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing 'prompt'"));
    }

    #[tokio::test]
    async fn race_criterion_llm_selects_winner() {
        use crate::adapter::mock::MockAdapter;

        // Mock adapter's judge() returns the first candidate (index 0)
        let adapter = MockAdapter::new();

        let candidates = vec!["output A".to_string(), "output B".to_string()];
        let result = evaluate_race_criterion_llm("Pick the best", &candidates, &adapter)
            .await
            .unwrap();

        // MockAdapter.judge returns 0 by default
        assert_eq!(result, 0);
    }

    #[tokio::test]
    async fn loop_condition_llm_continues() {
        use crate::adapter::mock::MockAdapter;
        use std::sync::Arc;

        let adapter = MockAdapter::new().with_default("yes");
        let ctx = Arc::new(ExecutionContext::new());

        let config = serde_json::json!({
            "prompt": "Should we continue?"
        });

        let result = evaluate_loop_condition_llm(&config, &ctx, &adapter)
            .await
            .unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn loop_condition_llm_stops() {
        use crate::adapter::mock::MockAdapter;
        use std::sync::Arc;

        let adapter = MockAdapter::new().with_default("no");
        let ctx = Arc::new(ExecutionContext::new());

        let config = serde_json::json!({
            "prompt": "Should we continue?"
        });

        let result = evaluate_loop_condition_llm(&config, &ctx, &adapter)
            .await
            .unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn loop_condition_llm_missing_prompt_errors() {
        use crate::adapter::mock::MockAdapter;
        use std::sync::Arc;

        let adapter = MockAdapter::new();
        let ctx = Arc::new(ExecutionContext::new());

        let config = serde_json::json!({});

        let result = evaluate_loop_condition_llm(&config, &ctx, &adapter).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn render_llm_prompt_interpolates() {
        use std::sync::Arc;

        let ctx = Arc::new(ExecutionContext::new());
        {
            let mut bb = ctx.blackboard();
            bb.set(
                "mode".into(),
                Value::String("fast".into()),
                BlackboardScope::Global,
            );
        }

        let mut inputs = Outputs::new();
        inputs.insert("data".into(), Value::String("test_value".into()));

        let result = render_llm_prompt(
            "Process {inputs.data} in {ctx.mode} mode",
            &inputs,
            &ctx,
        );

        assert_eq!(result, "Process test_value in fast mode");
    }
}
