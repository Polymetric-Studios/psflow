use crate::error::NodeError;
use crate::execute::concurrency::ConcurrencyLimits;
use crate::execute::context::{CancellationToken, ExecutionContext};
use crate::execute::event::ExecutionEvent;
use crate::execute::lifecycle::NodeState;
use crate::execute::topological::{
    cancel_downstream, collect_inputs, handle_branch_decision, is_branch_blocked,
    PassthroughHandler,
};
use crate::execute::{
    ExecutionError, ExecutionResult, Executor, HandlerRegistry, NodeHandler,
};
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
            });
        }

        let passthrough: Arc<dyn NodeHandler> = Arc::new(PassthroughHandler);

        // Find all nodes that are ready: not terminal, all predecessors terminal
        let ready_nodes: Vec<NodeId> = graph
            .nodes()
            .filter(|node| {
                let state = ctx.get_state(&node.id.0);
                if state.is_terminal() {
                    return false;
                }
                // All predecessors must be in terminal state
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
            });
        }

        let mut handles = Vec::new();
        let mut executed = Vec::new();

        for node_id in &ready_nodes {
            if ctx.is_cancelled() {
                break;
            }

            // Check if a predecessor failed/cancelled
            let has_failed_pred = graph.predecessors(node_id).iter().any(|pred| {
                matches!(
                    ctx.get_state(&pred.id.0),
                    NodeState::Failed | NodeState::Cancelled
                )
            });

            if has_failed_pred {
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
    use crate::graph::node::Node;
    use crate::graph::types::Value;
    use crate::execute::Outputs;
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
        g.add_node(Node::new("A", "A").with_handler("trace")).unwrap();
        g.add_node(Node::new("B", "B").with_handler("trace")).unwrap();
        g.add_node(Node::new("C", "C").with_handler("trace")).unwrap();
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
        g.add_node(Node::new("A", "A").with_handler("write_bb")).unwrap();
        g.add_node(Node::new("B", "B").with_handler("read_bb")).unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "write_bb".into(),
            sync_handler(|_, _| Ok(Outputs::new())),
        );
        handlers.insert(
            "read_bb".into(),
            sync_handler(|_, inputs| Ok(inputs)),
        );

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
            bb.get("injected", &crate::execute::blackboard::BlackboardScope::Global),
            Some(&Value::String("from_caller".into()))
        );
    }

    // -- Executor trait (run-to-completion) --

    #[tokio::test]
    async fn stepped_execute_runs_to_completion() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("trace")).unwrap();
        g.add_node(Node::new("B", "B").with_handler("trace")).unwrap();
        g.add_node(Node::new("C", "C").with_handler("trace")).unwrap();
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
        g.add_node(Node::new("B", "B").with_handler("fail")).unwrap();
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

        let result = SteppedExecutor::new()
            .execute(&g, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Failed);
        assert_eq!(result.node_states["C"], NodeState::Cancelled);
    }

    #[tokio::test]
    async fn stepped_cancellation() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("cancel_it")).unwrap();
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
        let result = executor
            .execute(&g, &HandlerRegistry::new())
            .await
            .unwrap();
        assert_eq!(result.node_states["A"], NodeState::Completed);
    }

    // -- Tick count verification --

    #[tokio::test]
    async fn tick_count_matches_graph_depth() {
        // A → B → C → D: 4 ticks
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("pass")).unwrap();
        g.add_node(Node::new("B", "B").with_handler("pass")).unwrap();
        g.add_node(Node::new("C", "C").with_handler("pass")).unwrap();
        g.add_node(Node::new("D", "D").with_handler("pass")).unwrap();
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
        g.add_node(Node::new("A", "A").with_handler("pass")).unwrap();

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
}
