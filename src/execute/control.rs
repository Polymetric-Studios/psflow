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
/// Supported expression forms:
/// - `"true"` / `"false"` — boolean literal
/// - `"inputs.key"` — lookup from node inputs, truthy check
/// - `"inputs.key == value"` — equality comparison
/// - `"inputs.key != value"` — inequality comparison
/// - `"ctx.key"` — lookup from blackboard (global scope), truthy check
/// - `"ctx.key == value"` — blackboard equality comparison
pub fn evaluate_guard(
    expr: &str,
    inputs: &Outputs,
    blackboard: &Blackboard,
) -> Result<GuardResult, NodeError> {
    let expr = expr.trim();

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
/// All nodes launch simultaneously. Waits for all to complete.
pub async fn execute_parallel(
    node_ids: &[NodeId],
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
) -> Result<(), ExecutionError> {
    let mut handles = Vec::new();

    for node_id in node_ids {
        if ctx.is_cancelled() || ctx.get_state(&node_id.0).is_terminal() {
            continue;
        }

        let handle =
            spawn_node_task(node_id.clone(), graph, handlers, ctx, passthrough.clone())?;
        handles.push(handle);
    }

    collect_results(handles, graph, ctx).await
}

/// Execute subgraph nodes as a race: first to complete successfully wins,
/// remaining siblings are aborted and marked cancelled.
///
/// Uses `futures::future::select_all` for true concurrent racing —
/// the winner is determined by actual completion time, not spawn order.
/// Returns the winning node's ID, or `None` if all failed.
pub async fn execute_race(
    node_ids: &[NodeId],
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
) -> Result<Option<NodeId>, ExecutionError> {
    if node_ids.is_empty() {
        return Ok(None);
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
    }
    Ok(())
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
    let result = if let Some(timeout) = timeout_dur {
        match tokio::time::timeout(timeout, handler.execute(node, inputs, cancel)).await {
            Ok(r) => r,
            Err(_) => Err(NodeError::Timeout {
                elapsed_ms: timeout.as_millis() as u64,
                limit_ms: timeout.as_millis() as u64,
            }),
        }
    } else {
        handler.execute(node, inputs, cancel).await
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

        let result = if let Some(timeout) = timeout_dur {
            match tokio::time::timeout(timeout, handler.execute(&node_clone, inputs, cancel)).await
            {
                Ok(r) => r,
                Err(_) => Err(NodeError::Timeout {
                    elapsed_ms: timeout.as_millis() as u64,
                    limit_ms: timeout.as_millis() as u64,
                }),
            }
        } else {
            handler.execute(&node_clone, inputs, cancel).await
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
    fn guard_missing_input_is_error() {
        let inputs = Outputs::new();
        let bb = Blackboard::new();
        assert!(evaluate_guard("inputs.missing", &inputs, &bb).is_err());
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
}
