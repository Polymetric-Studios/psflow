use crate::error::NodeError;
use crate::execute::concurrency::ConcurrencyLimits;
use crate::execute::context::{CancellationToken, ExecutionContext};
use crate::execute::event::ExecutionEvent;
use crate::execute::lifecycle::NodeState;
use crate::execute::topological::{
    cancel_downstream, collect_inputs, handle_branch_decision, is_branch_blocked,
    PassthroughHandler,
};
use crate::execute::{ExecutionError, ExecutionResult, Executor, HandlerRegistry, NodeHandler};
use crate::graph::node::NodeId;
use crate::graph::Graph;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Result of a single tick evaluation cycle.
#[derive(Debug)]
pub struct TickResult {
    /// Node IDs that executed during this tick.
    pub executed: Vec<String>,
    /// Whether all nodes have reached a terminal state.
    pub is_complete: bool,
    /// Node IDs that transitioned to `Suspended` during this tick.
    ///
    /// These nodes need external results via `ExecutionContext::submit_result()`
    /// before the graph can continue. The caller should collect results for
    /// each suspended node (keyed by node ID) and submit them before the
    /// next `tick()` call.
    pub suspended: Vec<String>,
}

/// Stepped/tick executor: advances the graph one evaluation cycle per `tick()` call.
///
/// Each tick finds all nodes whose inputs are satisfied and that haven't
/// been executed yet, runs them concurrently, and returns. Designed for
/// BT-style tick patterns, game-loop integration, and scenarios where
/// the caller needs to inspect or modify state between cycles.
///
/// Implements `Executor` (runs all ticks to completion) and also exposes
/// a `tick()` method for manual step-by-step control.
pub struct SteppedExecutor {
    cancel_token: CancellationToken,
    concurrency: ConcurrencyLimits,
}

impl SteppedExecutor {
    pub fn new() -> Self {
        Self {
            cancel_token: CancellationToken::new(),
            concurrency: ConcurrencyLimits::new(),
        }
    }

    pub fn with_cancel(token: CancellationToken) -> Self {
        Self {
            cancel_token: token,
            concurrency: ConcurrencyLimits::new(),
        }
    }

    pub fn with_concurrency(mut self, limits: ConcurrencyLimits) -> Self {
        self.concurrency = limits;
        self
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }

    /// Create a shared execution context for use with `tick()`.
    /// The caller holds onto this between tick calls to maintain state.
    pub fn create_context(&self) -> Arc<ExecutionContext> {
        Arc::new(ExecutionContext::with_concurrency(
            self.cancel_token.clone(),
            self.concurrency.clone(),
        ))
    }

    /// Run a single evaluation tick: execute all nodes whose inputs are satisfied.
    ///
    /// Returns which nodes executed and whether the graph is fully complete.
    /// Call repeatedly until `is_complete` is true, or inspect/modify the
    /// context between ticks for interactive control.
    pub async fn tick(
        &self,
        graph: &Graph,
        handlers: &HandlerRegistry,
        ctx: &Arc<ExecutionContext>,
    ) -> Result<TickResult, ExecutionError> {
        if ctx.is_cancelled() {
            cancel_all_pending(graph, ctx);
            return Ok(TickResult {
                executed: Vec::new(),
                is_complete: all_terminal(graph, ctx),
                suspended: Vec::new(),
            });
        }

        let passthrough: Arc<dyn NodeHandler> = Arc::new(PassthroughHandler);

        // Find all nodes that are ready: not terminal/suspended, all predecessors terminal
        let ready_nodes: Vec<NodeId> = graph
            .nodes()
            .filter(|node| {
                let state = ctx.get_state(&node.id.0);
                if state.is_terminal() || state.is_suspended() {
                    return false;
                }
                // All predecessors must be in terminal state (not just suspended)
                graph
                    .predecessors(&node.id)
                    .iter()
                    .all(|pred| ctx.get_state(&pred.id.0).is_terminal())
            })
            .map(|node| node.id.clone())
            .collect();

        if ready_nodes.is_empty() {
            return Ok(TickResult {
                executed: Vec::new(),
                is_complete: all_terminal(graph, ctx),
                suspended: ctx.suspended_nodes(),
            });
        }

        let mut handles = Vec::new();
        let mut executed = Vec::new();

        for node_id in &ready_nodes {
            if ctx.is_cancelled() {
                break;
            }

            // Check if predecessors failed/cancelled.
            // A convergence node (multiple predecessors) runs if ANY predecessor
            // completed — this supports conditional merge where one branch is
            // cancelled by design. A linear node (single predecessor) is
            // cancelled if that predecessor failed/cancelled.
            let preds = graph.predecessors(node_id);
            let all_failed = !preds.is_empty()
                && preds.iter().all(|pred| {
                    matches!(
                        ctx.get_state(&pred.id.0),
                        NodeState::Failed | NodeState::Cancelled
                    )
                });

            if all_failed {
                let _ = ctx.set_state(&node_id.0, NodeState::Cancelled);
                cancel_downstream(graph, node_id, ctx);
                executed.push(node_id.0.clone());
                continue;
            }

            if is_branch_blocked(graph, node_id, ctx) {
                let _ = ctx.set_state(&node_id.0, NodeState::Cancelled);
                cancel_downstream(graph, node_id, ctx);
                executed.push(node_id.0.clone());
                continue;
            }

            let node = graph.node(node_id).ok_or_else(|| {
                ExecutionError::ValidationFailed(format!("node '{}' not found", node_id))
            })?;

            // exec.activate == false: skip this node, passing inputs through as outputs.
            // Unlike Cancelled, Skipped does not propagate to downstream nodes.
            if node.exec.get("activate").and_then(|v| v.as_bool()) == Some(false) {
                let inputs = collect_inputs(graph, node_id, ctx);
                ctx.store_outputs(&node_id.0, inputs);
                let _ = ctx.set_state(&node_id.0, NodeState::Skipped);
                executed.push(node_id.0.clone());
                continue;
            }

            let handler: Arc<dyn NodeHandler> = node
                .handler
                .as_ref()
                .and_then(|name| handlers.get(name))
                .cloned()
                .unwrap_or_else(|| passthrough.clone());

            let inputs = collect_inputs(graph, node_id, ctx);
            let node_clone = node.clone();
            let node_id_str = node_id.0.clone();
            let cancel = ctx.cancel_token().clone();
            let ctx_clone = ctx.clone();

            let timeout_dur = node
                .exec
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .map(Duration::from_millis);

            let retry_config = crate::execute::retry::RetryConfig::from_exec(&node.exec);

            let _global_permit = ctx.concurrency().acquire().await;

            ctx.set_state(&node_id_str, NodeState::Pending)
                .map_err(|e| ExecutionError::ValidationFailed(e.to_string()))?;

            executed.push(node_id_str.clone());

            handles.push(tokio::spawn(async move {
                let _permit = _global_permit;

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
                        crate::execute::retry::execute_with_retry_ctx(
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
            }));
        }

        let mut tick_suspended = Vec::new();

        for handle in handles {
            let (node_id, outcome) = handle
                .await
                .map_err(|e| ExecutionError::ValidationFailed(format!("task panic: {e}")))?;

            let nid = NodeId::new(&node_id);

            match outcome {
                Ok(outputs) => {
                    handle_branch_decision(graph, &node_id, &outputs, ctx, None).await;
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
                Err(NodeError::Suspended { .. }) => {
                    let _ = ctx.set_state(&node_id, NodeState::Suspended);
                    tick_suspended.push(node_id);
                }
                Err(ref error) => {
                    ctx.emit(ExecutionEvent::NodeFailed {
                        node_id: node_id.clone(),
                        error: error.clone(),
                    });
                    let _ = ctx.set_state(&node_id, NodeState::Failed);
                    cancel_downstream(graph, &nid, ctx);
                }
            }
        }

        Ok(TickResult {
            executed,
            is_complete: all_terminal(graph, ctx),
            suspended: tick_suspended,
        })
    }
}

impl Default for SteppedExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for SteppedExecutor {
    fn execute<'a>(
        &'a self,
        graph: &'a Graph,
        handlers: &'a HandlerRegistry,
    ) -> Pin<Box<dyn Future<Output = Result<ExecutionResult, ExecutionError>> + Send + 'a>> {
        Box::pin(async move {
            let start = Instant::now();
            let ctx = self.create_context();

            ctx.emit(ExecutionEvent::ExecutionStarted { timestamp: start });

            if graph.node_count() == 0 {
                let elapsed = start.elapsed();
                ctx.emit(ExecutionEvent::ExecutionCompleted { elapsed });
                return Ok(ExecutionResult {
                    node_states: HashMap::new(),
                    node_outputs: HashMap::new(),
                    events: ctx.take_events(),
                    elapsed,
                });
            }

            loop {
                let tick_result = self.tick(graph, handlers, &ctx).await?;
                if tick_result.is_complete {
                    break;
                }
                if !tick_result.suspended.is_empty() {
                    // Nodes are waiting for external results — the auto-run
                    // Executor cannot provide them. Stop without cancelling;
                    // the caller should use tick() directly for suspended flows.
                    break;
                }
                if tick_result.executed.is_empty() {
                    // No progress — all remaining nodes are blocked or orphaned.
                    // Mark them cancelled to avoid infinite loop.
                    cancel_all_pending(graph, &ctx);
                    break;
                }
            }

            let elapsed = start.elapsed();
            ctx.emit(ExecutionEvent::ExecutionCompleted { elapsed });

            Ok(ExecutionResult {
                node_states: ctx.take_node_states(),
                node_outputs: ctx.take_node_outputs(),
                events: ctx.take_events(),
                elapsed,
            })
        })
    }
}

/// Check if all nodes in the graph are in terminal state.
fn all_terminal(graph: &Graph, ctx: &ExecutionContext) -> bool {
    graph
        .nodes()
        .all(|node| ctx.get_state(&node.id.0).is_terminal())
}

/// Cancel all nodes that are not yet in a terminal state.
fn cancel_all_pending(graph: &Graph, ctx: &ExecutionContext) {
    for node in graph.nodes() {
        if !ctx.get_state(&node.id.0).is_terminal() {
            let _ = ctx.set_state(&node.id.0, NodeState::Cancelled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::sync_handler;
    use crate::execute::Outputs;
    use crate::graph::node::Node;
    use crate::graph::types::Value;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering as AtomicOrdering;

    fn trace_handler() -> Arc<dyn NodeHandler> {
        sync_handler(|node, inputs| {
            let prev = inputs
                .get("trace")
                .and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let mut outputs = Outputs::new();
            outputs.insert("trace".into(), Value::String(format!("{prev}{}", node.id)));
            Ok(outputs)
        })
    }

    fn trace_handlers() -> HandlerRegistry {
        let mut h = HandlerRegistry::new();
        h.insert("trace".into(), trace_handler());
        h
    }

    // -- Manual tick tests --

    #[tokio::test]
    async fn tick_advances_one_wave() {
        // A → B → C: three ticks to complete
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("trace"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("trace"))
            .unwrap();
        g.add_node(Node::new("C", "C").with_handler("trace"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();

        let executor = SteppedExecutor::new();
        let ctx = executor.create_context();
        let handlers = trace_handlers();

        // Tick 1: A fires
        let t1 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t1.executed, vec!["A"]);
        assert!(!t1.is_complete);
        assert_eq!(ctx.get_state("A"), NodeState::Completed);
        assert_eq!(ctx.get_state("B"), NodeState::Idle);

        // Tick 2: B fires
        let t2 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t2.executed, vec!["B"]);
        assert!(!t2.is_complete);

        // Tick 3: C fires
        let t3 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t3.executed, vec!["C"]);
        assert!(t3.is_complete);

        assert_eq!(
            ctx.get_outputs("C").unwrap()["trace"],
            Value::String("ABC".into())
        );
    }

    #[tokio::test]
    async fn tick_parallel_nodes_in_single_tick() {
        // A → (B, C) → D: B and C should fire in the same tick
        let mut g = Graph::new();
        for id in ["A", "B", "C", "D"] {
            g.add_node(Node::new(id, id).with_handler("pass")).unwrap();
        }
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"C".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"D".into(), "", None).unwrap();
        g.add_edge(&"C".into(), "", &"D".into(), "", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));

        let executor = SteppedExecutor::new();
        let ctx = executor.create_context();

        // Tick 1: A
        let t1 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t1.executed.len(), 1);
        assert!(!t1.is_complete);

        // Tick 2: B and C in parallel
        let t2 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t2.executed.len(), 2);
        assert!(t2.executed.contains(&"B".to_string()));
        assert!(t2.executed.contains(&"C".to_string()));
        assert!(!t2.is_complete);

        // Tick 3: D
        let t3 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t3.executed.len(), 1);
        assert!(t3.is_complete);
    }

    #[tokio::test]
    async fn tick_context_inspection_between_ticks() {
        // Verify the caller can inspect/modify blackboard between ticks
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("write_bb"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("read_bb"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("write_bb".into(), sync_handler(|_, _| Ok(Outputs::new())));
        handlers.insert("read_bb".into(), sync_handler(|_, inputs| Ok(inputs)));

        let executor = SteppedExecutor::new();
        let ctx = executor.create_context();

        // Tick 1: A executes
        executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(ctx.get_state("A"), NodeState::Completed);

        // Between ticks: inspect state, modify blackboard
        {
            use crate::execute::blackboard::BlackboardScope;
            let mut bb = ctx.blackboard();
            bb.set(
                "injected".into(),
                Value::String("from_caller".into()),
                BlackboardScope::Global,
            );
        }

        // Tick 2: B executes — blackboard modification is visible
        executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(ctx.get_state("B"), NodeState::Completed);

        let bb = ctx.blackboard();
        assert_eq!(
            bb.get(
                "injected",
                &crate::execute::blackboard::BlackboardScope::Global
            ),
            Some(&Value::String("from_caller".into()))
        );
    }

    // -- Executor trait (run-to-completion) --

    #[tokio::test]
    async fn stepped_execute_runs_to_completion() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("trace"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("trace"))
            .unwrap();
        g.add_node(Node::new("C", "C").with_handler("trace"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();

        let result = SteppedExecutor::new()
            .execute(&g, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Completed);
        assert_eq!(result.node_states["C"], NodeState::Completed);
        assert_eq!(
            result.node_outputs["C"]["trace"],
            Value::String("ABC".into())
        );
    }

    #[tokio::test]
    async fn stepped_empty_graph() {
        let result = SteppedExecutor::new()
            .execute(&Graph::new(), &HandlerRegistry::new())
            .await
            .unwrap();
        assert!(result.node_states.is_empty());
    }

    #[tokio::test]
    async fn stepped_error_cascades() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("ok")).unwrap();
        g.add_node(Node::new("B", "B").with_handler("fail"))
            .unwrap();
        g.add_node(Node::new("C", "C").with_handler("ok")).unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("ok".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "fail".into(),
            sync_handler(|_, _| {
                Err(NodeError::Failed {
                    source_message: None,
                    message: "intentional".into(),
                    recoverable: false,
                })
            }),
        );

        let result = SteppedExecutor::new().execute(&g, &handlers).await.unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Failed);
        assert_eq!(result.node_states["C"], NodeState::Cancelled);
    }

    #[tokio::test]
    async fn stepped_cancellation() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("cancel_it"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("ok")).unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();

        let token = CancellationToken::new();
        let cancel_clone = token.clone();
        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "cancel_it".into(),
            sync_handler(move |_, inputs| {
                cancel_clone.cancel();
                Ok(inputs)
            }),
        );
        handlers.insert("ok".into(), sync_handler(|_, inputs| Ok(inputs)));

        let result = SteppedExecutor::with_cancel(token)
            .execute(&g, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Cancelled);
    }

    // -- Executor trait conformance --

    #[tokio::test]
    async fn stepped_executor_trait_conformance() {
        let executor: Box<dyn Executor> = Box::new(SteppedExecutor::new());

        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        let result = executor.execute(&g, &HandlerRegistry::new()).await.unwrap();
        assert_eq!(result.node_states["A"], NodeState::Completed);
    }

    // -- Tick count verification --

    #[tokio::test]
    async fn tick_count_matches_graph_depth() {
        // A → B → C → D: 4 ticks
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("C", "C").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("D", "D").with_handler("pass"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();
        g.add_edge(&"C".into(), "", &"D".into(), "", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));

        let executor = SteppedExecutor::new();
        let ctx = executor.create_context();
        let mut tick_count = 0;

        loop {
            let result = executor.tick(&g, &handlers, &ctx).await.unwrap();
            if result.executed.is_empty() && result.is_complete {
                break;
            }
            tick_count += 1;
            if result.is_complete {
                break;
            }
        }

        assert_eq!(tick_count, 4);
    }

    // -- Tick after completion --

    #[tokio::test]
    async fn tick_after_completion_returns_empty() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("pass"))
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));

        let executor = SteppedExecutor::new();
        let ctx = executor.create_context();

        // First tick completes the only node
        let t1 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert!(t1.is_complete);

        // Additional ticks should return empty with is_complete still true
        let t2 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert!(t2.executed.is_empty());
        assert!(t2.is_complete);
    }

    // -- Concurrency limits --

    #[tokio::test]
    async fn stepped_concurrency_limit_respected() {
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        // 4 independent nodes with max_parallelism: 2
        let mut g = Graph::new();
        for i in 0..4 {
            g.add_node(Node::new(format!("N{i}"), format!("N{i}")).with_handler("track"))
                .unwrap();
        }

        let c = concurrent.clone();
        let m = max_seen.clone();
        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "track".into(),
            Arc::new(ConcurrencyTracker(c, m)) as Arc<dyn NodeHandler>,
        );

        let result = SteppedExecutor::new()
            .with_concurrency(ConcurrencyLimits::with_max_parallelism(2))
            .execute(&g, &handlers)
            .await
            .unwrap();

        for i in 0..4 {
            assert_eq!(result.node_states[&format!("N{i}")], NodeState::Completed);
        }
        assert!(
            max_seen.load(AtomicOrdering::SeqCst) <= 2,
            "max concurrent was {}, expected <= 2",
            max_seen.load(AtomicOrdering::SeqCst)
        );
    }

    struct ConcurrencyTracker(Arc<AtomicUsize>, Arc<AtomicUsize>);

    impl NodeHandler for ConcurrencyTracker {
        fn execute(
            &self,
            _node: &Node,
            _inputs: Outputs,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
            let concurrent = self.0.clone();
            let max_seen = self.1.clone();
            Box::pin(async move {
                let cur = concurrent.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                max_seen.fetch_max(cur, AtomicOrdering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                concurrent.fetch_sub(1, AtomicOrdering::SeqCst);
                Ok(Outputs::new())
            })
        }
    }

    // -- Conditional branching (Phase 2d verification) --

    #[tokio::test]
    async fn conditional_guard_selects_branch() {
        // check --> branch
        // branch -->|yes| deploy  (guard evaluates to true → edge_label "yes")
        // branch -->|else| wait
        // deploy --> done
        // wait --> done
        let mut g = Graph::new();
        g.add_node(Node::new("check", "check").with_handler("pass"))
            .unwrap();
        let mut branch = Node::new("branch", "branch").with_handler("pass");
        branch.config = serde_json::json!({"guard": "true"});
        g.add_node(branch).unwrap();
        g.add_node(Node::new("deploy", "deploy").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("wait", "wait").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("done", "done").with_handler("pass"))
            .unwrap();

        g.add_edge(&"check".into(), "", &"branch".into(), "", None)
            .unwrap();
        g.add_edge(
            &"branch".into(),
            "",
            &"deploy".into(),
            "",
            Some("yes".into()),
        )
        .unwrap();
        g.add_edge(
            &"branch".into(),
            "",
            &"wait".into(),
            "",
            Some("else".into()),
        )
        .unwrap();
        g.add_edge(&"deploy".into(), "", &"done".into(), "", None)
            .unwrap();
        g.add_edge(&"wait".into(), "", &"done".into(), "", None)
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));

        let result = SteppedExecutor::new().execute(&g, &handlers).await.unwrap();

        assert_eq!(result.node_states["check"], NodeState::Completed);
        assert_eq!(result.node_states["branch"], NodeState::Completed);
        assert_eq!(result.node_states["deploy"], NodeState::Completed);
        assert_eq!(result.node_states["wait"], NodeState::Cancelled);
        assert_eq!(result.node_states["done"], NodeState::Completed);
    }

    #[tokio::test]
    async fn conditional_else_fallback() {
        // branch -->|yes| a
        // branch -->|else| b
        // guard = "false" → edge_label "no", doesn't match "yes", so else runs
        let mut g = Graph::new();
        let mut branch = Node::new("branch", "branch").with_handler("pass");
        branch.config = serde_json::json!({"guard": "false"});
        g.add_node(branch).unwrap();
        g.add_node(Node::new("a", "a").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("b", "b").with_handler("pass"))
            .unwrap();

        g.add_edge(&"branch".into(), "", &"a".into(), "", Some("yes".into()))
            .unwrap();
        g.add_edge(&"branch".into(), "", &"b".into(), "", Some("else".into()))
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));

        let result = SteppedExecutor::new().execute(&g, &handlers).await.unwrap();

        assert_eq!(result.node_states["branch"], NodeState::Completed);
        assert_eq!(result.node_states["a"], NodeState::Cancelled);
        assert_eq!(result.node_states["b"], NodeState::Completed);
    }

    #[tokio::test]
    async fn conditional_three_branches_else_fallback() {
        // branch -->|yes| a
        // branch -->|no| b
        // branch -->|else| c
        // guard = "\"maybe\"" → no label matches, else runs
        let mut g = Graph::new();
        let mut branch = Node::new("branch", "branch").with_handler("pass");
        branch.config = serde_json::json!({"guard": "\"maybe\""});
        g.add_node(branch).unwrap();
        g.add_node(Node::new("a", "a").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("b", "b").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("c", "c").with_handler("pass"))
            .unwrap();

        g.add_edge(&"branch".into(), "", &"a".into(), "", Some("yes".into()))
            .unwrap();
        g.add_edge(&"branch".into(), "", &"b".into(), "", Some("no".into()))
            .unwrap();
        g.add_edge(&"branch".into(), "", &"c".into(), "", Some("else".into()))
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));

        let result = SteppedExecutor::new().execute(&g, &handlers).await.unwrap();

        assert_eq!(result.node_states["branch"], NodeState::Completed);
        assert_eq!(result.node_states["a"], NodeState::Cancelled);
        assert_eq!(result.node_states["b"], NodeState::Cancelled);
        assert_eq!(result.node_states["c"], NodeState::Completed);
    }

    // -- Selective activation (exec.activate) --

    fn node_with_activate(id: &str, activate: bool) -> Node {
        let mut node = Node::new(id, id).with_handler("trace");
        node.exec = serde_json::json!({"activate": activate});
        node
    }

    #[tokio::test]
    async fn skipped_node_passes_through_inputs() {
        // A → B (skipped) → C: C should receive A's output
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("trace"))
            .unwrap();
        g.add_node(node_with_activate("B", false)).unwrap();
        g.add_node(Node::new("C", "C").with_handler("trace"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();

        let result = SteppedExecutor::new()
            .execute(&g, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Skipped);
        assert_eq!(result.node_states["C"], NodeState::Completed);
        // C sees A→B→C trace (B skipped so only A and C appended)
        assert_eq!(
            result.node_outputs["C"]["trace"],
            Value::String("AC".into())
        );
    }

    #[tokio::test]
    async fn all_middle_nodes_skipped() {
        // A → B (skipped) → C (skipped) → D: D receives A's output
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("trace"))
            .unwrap();
        g.add_node(node_with_activate("B", false)).unwrap();
        g.add_node(node_with_activate("C", false)).unwrap();
        g.add_node(Node::new("D", "D").with_handler("trace"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();
        g.add_edge(&"C".into(), "", &"D".into(), "", None).unwrap();

        let result = SteppedExecutor::new()
            .execute(&g, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Skipped);
        assert_eq!(result.node_states["C"], NodeState::Skipped);
        assert_eq!(result.node_states["D"], NodeState::Completed);
        assert_eq!(
            result.node_outputs["D"]["trace"],
            Value::String("AD".into())
        );
    }

    #[tokio::test]
    async fn single_node_skipped() {
        // Single node with activate=false
        let mut g = Graph::new();
        g.add_node(node_with_activate("A", false)).unwrap();

        let result = SteppedExecutor::new()
            .execute(&g, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Skipped);
    }

    #[tokio::test]
    async fn all_nodes_skipped() {
        // A (skipped) → B (skipped): both should be Skipped
        let mut g = Graph::new();
        g.add_node(node_with_activate("A", false)).unwrap();
        g.add_node(node_with_activate("B", false)).unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();

        let result = SteppedExecutor::new()
            .execute(&g, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Skipped);
        assert_eq!(result.node_states["B"], NodeState::Skipped);
    }

    #[tokio::test]
    async fn skipped_node_does_not_cancel_downstream() {
        // Verify Skipped differs from Cancelled: downstream runs
        let mut g = Graph::new();
        g.add_node(node_with_activate("A", false)).unwrap();
        g.add_node(Node::new("B", "B").with_handler("trace"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();

        let result = SteppedExecutor::new()
            .execute(&g, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Skipped);
        assert_eq!(result.node_states["B"], NodeState::Completed);
    }

    #[tokio::test]
    async fn activate_true_runs_normally() {
        // exec.activate = true should behave identically to no annotation
        let mut g = Graph::new();
        let mut node = Node::new("A", "A").with_handler("trace");
        node.exec = serde_json::json!({"activate": true});
        g.add_node(node).unwrap();

        let result = SteppedExecutor::new()
            .execute(&g, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
    }

    #[tokio::test]
    async fn skipped_in_feedback_loop_intermediary() {
        // A → B (skipped) → C, A also feeds C directly (convergence)
        // C should run once both A (Completed) and B (Skipped) are terminal
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("trace"))
            .unwrap();
        g.add_node(node_with_activate("B", false)).unwrap();
        g.add_node(Node::new("C", "C").with_handler("trace"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"C".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();

        let result = SteppedExecutor::new()
            .execute(&g, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Skipped);
        assert_eq!(result.node_states["C"], NodeState::Completed);
    }

    #[tokio::test]
    async fn convergence_one_branch_fails_other_succeeds() {
        // A --> B (fails)
        // A --> C (succeeds)
        // B --> D (convergence)
        // C --> D
        // D should run because C succeeded
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("fail"))
            .unwrap();
        g.add_node(Node::new("C", "C").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("D", "D").with_handler("pass"))
            .unwrap();

        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"C".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"D".into(), "", None).unwrap();
        g.add_edge(&"C".into(), "", &"D".into(), "", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "fail".into(),
            sync_handler(|_, _| {
                Err(NodeError::Failed {
                    source_message: None,
                    message: "intentional".into(),
                    recoverable: false,
                })
            }),
        );

        let result = SteppedExecutor::new().execute(&g, &handlers).await.unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Failed);
        assert_eq!(result.node_states["C"], NodeState::Completed);
        assert_eq!(result.node_states["D"], NodeState::Completed);
    }

    // -- Suspended node tests --

    /// Handler that returns NodeError::Suspended, simulating an external-await node.
    fn suspend_handler() -> Arc<dyn NodeHandler> {
        Arc::new(SuspendHandler)
    }

    struct SuspendHandler;

    impl NodeHandler for SuspendHandler {
        fn execute(
            &self,
            _node: &Node,
            _inputs: Outputs,
            _cancel: crate::execute::CancellationToken,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Outputs, NodeError>> + Send>> {
            Box::pin(async {
                Err(NodeError::Suspended {
                    reason: "awaiting external result".into(),
                })
            })
        }
    }

    #[tokio::test]
    async fn tick_suspends_node_and_reports_it() {
        // A(suspend) → B: A should suspend, B should not fire
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("suspend"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("trace"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();

        let mut handlers = trace_handlers();
        handlers.insert("suspend".into(), suspend_handler());

        let executor = SteppedExecutor::new();
        let ctx = executor.create_context();

        // Tick 1: A fires and suspends
        let t1 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t1.executed, vec!["A"]);
        assert_eq!(t1.suspended, vec!["A"]);
        assert!(!t1.is_complete); // Graph is NOT complete while A is suspended

        // Tick 2: Nothing can fire (B depends on A, A is suspended)
        let t2 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert!(t2.executed.is_empty());
        assert!(!t2.is_complete);
        // Still reports A as suspended in the context
        assert_eq!(ctx.suspended_nodes(), vec!["A"]);
    }

    #[tokio::test]
    async fn submit_result_unblocks_downstream() {
        // A(suspend) → B: submit result for A, then B fires
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("suspend"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("trace"))
            .unwrap();
        g.add_edge(&"A".into(), "trace", &"B".into(), "trace", None)
            .unwrap();

        let mut handlers = trace_handlers();
        handlers.insert("suspend".into(), suspend_handler());

        let executor = SteppedExecutor::new();
        let ctx = executor.create_context();

        // Tick 1: A suspends
        let t1 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t1.suspended, vec!["A"]);
        assert!(!t1.is_complete);

        // Submit external result for A
        let mut result_outputs = Outputs::new();
        result_outputs.insert("trace".into(), Value::String("external-".into()));
        ctx.submit_result("A", result_outputs).unwrap();

        assert_eq!(ctx.get_state("A"), NodeState::Completed);
        assert!(ctx.suspended_nodes().is_empty());

        // Tick 2: B fires with A's submitted outputs
        let t2 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t2.executed, vec!["B"]);
        assert!(t2.is_complete);

        // B should see A's submitted output
        let b_out = ctx.get_outputs("B").unwrap();
        assert_eq!(
            b_out.get("trace"),
            Some(&Value::String("external-B".into()))
        );
    }

    #[tokio::test]
    async fn parallel_suspend_with_keyed_results() {
        // S → (A(suspend), B(suspend)) → D
        // Both A and B suspend, caller submits separate keyed results
        let mut g = Graph::new();
        g.add_node(Node::new("S", "S").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("A", "A").with_handler("suspend"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("suspend"))
            .unwrap();
        g.add_node(Node::new("D", "D").with_handler("pass"))
            .unwrap();
        g.add_edge(&"S".into(), "", &"A".into(), "", None).unwrap();
        g.add_edge(&"S".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "review", &"D".into(), "a_review", None)
            .unwrap();
        g.add_edge(&"B".into(), "review", &"D".into(), "b_review", None)
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert("suspend".into(), suspend_handler());

        let executor = SteppedExecutor::new();
        let ctx = executor.create_context();

        // Tick 1: S fires
        let t1 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t1.executed, vec!["S"]);
        assert!(t1.suspended.is_empty());

        // Tick 2: A and B fire and both suspend
        let t2 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t2.executed.len(), 2);
        assert_eq!(t2.suspended.len(), 2);
        assert!(t2.suspended.contains(&"A".to_string()));
        assert!(t2.suspended.contains(&"B".to_string()));
        assert!(!t2.is_complete);

        // Submit separate keyed results for A and B
        let mut keyed = HashMap::new();

        let mut out_a = Outputs::new();
        out_a.insert("review".into(), Value::String("ux-feedback".into()));
        keyed.insert("A".into(), out_a);

        let mut out_b = Outputs::new();
        out_b.insert("review".into(), Value::String("visual-feedback".into()));
        keyed.insert("B".into(), out_b);

        ctx.submit_results(keyed).unwrap();

        assert_eq!(ctx.get_state("A"), NodeState::Completed);
        assert_eq!(ctx.get_state("B"), NodeState::Completed);

        // Tick 3: D fires with both A and B's distinct results
        let t3 = executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(t3.executed, vec!["D"]);
        assert!(t3.is_complete);

        // D should have received both keyed results, not duplicated
        let d_out = ctx.get_outputs("D").unwrap();
        assert_eq!(
            d_out.get("a_review"),
            Some(&Value::String("ux-feedback".into()))
        );
        assert_eq!(
            d_out.get("b_review"),
            Some(&Value::String("visual-feedback".into()))
        );
    }

    #[tokio::test]
    async fn executor_trait_stops_on_suspended_nodes() {
        // When using the Executor trait (auto-run), it should stop when
        // nodes are suspended rather than spinning forever.
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("suspend"))
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("suspend".into(), suspend_handler());

        let result = SteppedExecutor::new().execute(&g, &handlers).await.unwrap();

        // A should be suspended (not completed, not cancelled)
        assert_eq!(result.node_states["A"], NodeState::Suspended);
    }

    #[tokio::test]
    async fn suspended_node_not_re_executed_on_tick() {
        // Verify that a suspended node is not picked up again for execution
        let execution_count = Arc::new(AtomicUsize::new(0));
        let count_clone = execution_count.clone();

        let counting_suspend = Arc::new({
            struct CountingSuspend(Arc<AtomicUsize>);
            impl NodeHandler for CountingSuspend {
                fn execute(
                    &self,
                    _node: &Node,
                    _inputs: Outputs,
                    _cancel: crate::execute::CancellationToken,
                ) -> Pin<Box<dyn std::future::Future<Output = Result<Outputs, NodeError>> + Send>>
                {
                    self.0.fetch_add(1, AtomicOrdering::SeqCst);
                    Box::pin(async {
                        Err(NodeError::Suspended {
                            reason: "waiting".into(),
                        })
                    })
                }
            }
            CountingSuspend(count_clone)
        });

        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("counting"))
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("counting".into(), counting_suspend as Arc<dyn NodeHandler>);

        let executor = SteppedExecutor::new();
        let ctx = executor.create_context();

        // Tick 1: A fires and suspends
        executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(execution_count.load(AtomicOrdering::SeqCst), 1);

        // Tick 2: A should NOT be re-executed
        executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(execution_count.load(AtomicOrdering::SeqCst), 1);

        // Tick 3: Still not re-executed
        executor.tick(&g, &handlers, &ctx).await.unwrap();
        assert_eq!(execution_count.load(AtomicOrdering::SeqCst), 1);
    }
}
