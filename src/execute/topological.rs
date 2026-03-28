use crate::error::NodeError;
use crate::execute::context::{CancellationToken, ExecutionContext};
use crate::execute::event::ExecutionEvent;
use crate::execute::lifecycle::NodeState;
use crate::execute::{
    ExecutionError, ExecutionResult, Executor, HandlerRegistry, NodeHandler, Outputs,
};
use crate::graph::node::NodeId;
use crate::graph::Graph;
use petgraph::algo::toposort;
use petgraph::stable_graph::NodeIndex;
use petgraph::Direction;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Executes graphs in dependency order, running independent nodes in parallel waves.
pub struct TopologicalExecutor {
    cancel_token: CancellationToken,
}

impl TopologicalExecutor {
    pub fn new() -> Self {
        Self {
            cancel_token: CancellationToken::new(),
        }
    }

    pub fn with_cancel(token: CancellationToken) -> Self {
        Self {
            cancel_token: token,
        }
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }
}

impl Default for TopologicalExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for TopologicalExecutor {
    fn execute<'a>(
        &'a self,
        graph: &'a Graph,
        handlers: &'a HandlerRegistry,
    ) -> Pin<Box<dyn Future<Output = Result<ExecutionResult, ExecutionError>> + Send + 'a>> {
        Box::pin(execute_impl(graph, handlers, self.cancel_token.clone()))
    }
}

async fn execute_impl(
    graph: &Graph,
    handlers: &HandlerRegistry,
    cancel_token: CancellationToken,
) -> Result<ExecutionResult, ExecutionError> {
    let start = Instant::now();
    let ctx = Arc::new(ExecutionContext::with_cancel(cancel_token));

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

    let waves = compute_waves(graph)?;
    let passthrough: Arc<dyn NodeHandler> = Arc::new(PassthroughHandler);

    for wave in &waves {
        // Check global cancellation before starting a new wave
        if ctx.is_cancelled() {
            for node_id in wave {
                if !ctx.get_state(&node_id.0).is_terminal() {
                    let _ = ctx.set_state(&node_id.0, NodeState::Cancelled);
                }
            }
            continue;
        }

        let mut handles = Vec::new();

        for node_id in wave {
            if ctx.get_state(&node_id.0).is_terminal() {
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

            let inputs = collect_inputs(graph, node_id, &ctx);
            let node_clone = node.clone();
            let node_id_str = node_id.0.clone();
            let cancel = ctx.cancel_token().clone();
            let ctx_clone = ctx.clone();

            // Per-node timeout from exec.timeout_ms annotation
            let timeout_dur = node
                .exec
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .map(Duration::from_millis);

            // Transition to Pending (in the main loop, before spawn)
            ctx.set_state(&node_id_str, NodeState::Pending)
                .map_err(|e| ExecutionError::ValidationFailed(e.to_string()))?;

            handles.push(tokio::spawn(async move {
                // Check cancellation before running
                if cancel.is_cancelled() {
                    return (
                        node_id_str,
                        Err(NodeError::Cancelled {
                            reason: "execution cancelled".into(),
                        }),
                    );
                }

                // Transition to Running inside the spawned task
                if let Err(e) = ctx_clone.set_state(&node_id_str, NodeState::Running) {
                    return (node_id_str, Err(e));
                }

                // Execute with optional timeout
                let result = if let Some(timeout) = timeout_dur {
                    match tokio::time::timeout(
                        timeout,
                        handler.execute(&node_clone, inputs, cancel),
                    )
                    .await
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
            }));
        }

        // Await all tasks in this wave
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
                    cancel_downstream(graph, &NodeId::new(&node_id), &ctx);
                }
            }
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
}

/// Group nodes into parallel waves based on dependency depth.
pub(crate) fn compute_waves(graph: &Graph) -> Result<Vec<Vec<NodeId>>, ExecutionError> {
    let topo = toposort(&graph.inner, None).map_err(|cycle| {
        ExecutionError::ValidationFailed(format!(
            "cycle detected at node '{}'",
            graph.inner[cycle.node_id()].id
        ))
    })?;

    let mut node_wave: HashMap<NodeIndex, usize> = HashMap::new();
    let mut waves: Vec<Vec<NodeId>> = Vec::new();

    for &idx in &topo {
        let wave = graph
            .inner
            .neighbors_directed(idx, Direction::Incoming)
            .filter_map(|pred| node_wave.get(&pred))
            .map(|&w| w + 1)
            .max()
            .unwrap_or(0);

        node_wave.insert(idx, wave);
        while waves.len() <= wave {
            waves.push(Vec::new());
        }
        waves[wave].push(graph.inner[idx].id.clone());
    }

    Ok(waves)
}

/// Collect inputs for a node from upstream node outputs via edge port mapping.
fn collect_inputs(graph: &Graph, node_id: &NodeId, ctx: &ExecutionContext) -> Outputs {
    let mut inputs = Outputs::new();
    for (src_node, edge_data) in graph.incoming_edges(node_id) {
        if let Some(src_outputs) = ctx.get_outputs(&src_node.id.0) {
            if !edge_data.source_port.is_empty() {
                if let Some(value) = src_outputs.get(&edge_data.source_port) {
                    let key = if edge_data.target_port.is_empty() {
                        &edge_data.source_port
                    } else {
                        &edge_data.target_port
                    };
                    inputs.insert(key.clone(), value.clone());
                }
            } else {
                // No port specified — merge all source outputs (last-writer-wins on key conflict)
                inputs.extend(src_outputs);
            }
        }
    }
    inputs
}

/// Mark all downstream (transitive successors) of a failed node as Cancelled.
fn cancel_downstream(graph: &Graph, failed_id: &NodeId, ctx: &ExecutionContext) {
    let mut stack = vec![failed_id.clone()];
    let mut visited = HashSet::new();
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

/// Default handler that passes inputs through as outputs unchanged.
struct PassthroughHandler;

impl NodeHandler for PassthroughHandler {
    fn execute(
        &self,
        _node: &crate::graph::node::Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        Box::pin(async move { Ok(inputs) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::sync_handler;
    use crate::graph::node::Node;
    use crate::graph::types::Value;

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

    fn linear_graph() -> Graph {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("trace"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("trace"))
            .unwrap();
        g.add_node(Node::new("C", "C").with_handler("trace"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();
        g
    }

    // -- Core execution tests --

    #[tokio::test]
    async fn execute_linear_chain() {
        let graph = linear_graph();
        let result = TopologicalExecutor::new()
            .execute(&graph, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Completed);
        assert_eq!(result.node_states["C"], NodeState::Completed);
        assert_eq!(result.node_outputs["C"]["trace"], Value::String("ABC".into()));
    }

    #[tokio::test]
    async fn execute_diamond_dependency() {
        let mut graph = Graph::new();
        for id in ["A", "B", "C", "D"] {
            graph
                .add_node(Node::new(id, id).with_handler("pass"))
                .unwrap();
        }
        graph.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        graph.add_edge(&"A".into(), "", &"C".into(), "", None).unwrap();
        graph.add_edge(&"B".into(), "", &"D".into(), "", None).unwrap();
        graph.add_edge(&"C".into(), "", &"D".into(), "", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        for state in result.node_states.values() {
            assert_eq!(*state, NodeState::Completed);
        }
    }

    #[tokio::test]
    async fn execute_fan_out_fan_in() {
        // A → (B, C, D) → E
        let mut graph = Graph::new();
        for id in ["A", "B", "C", "D", "E"] {
            graph
                .add_node(Node::new(id, id).with_handler("trace"))
                .unwrap();
        }
        graph.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        graph.add_edge(&"A".into(), "", &"C".into(), "", None).unwrap();
        graph.add_edge(&"A".into(), "", &"D".into(), "", None).unwrap();
        graph.add_edge(&"B".into(), "", &"E".into(), "", None).unwrap();
        graph.add_edge(&"C".into(), "", &"E".into(), "", None).unwrap();
        graph.add_edge(&"D".into(), "", &"E".into(), "", None).unwrap();

        let result = TopologicalExecutor::new()
            .execute(&graph, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states.len(), 5);
        for state in result.node_states.values() {
            assert_eq!(*state, NodeState::Completed);
        }
    }

    #[tokio::test]
    async fn execute_empty_graph() {
        let result = TopologicalExecutor::new()
            .execute(&Graph::new(), &HandlerRegistry::new())
            .await
            .unwrap();
        assert!(result.node_states.is_empty());
    }

    #[tokio::test]
    async fn single_node_no_handler_uses_passthrough() {
        let mut graph = Graph::new();
        graph.add_node(Node::new("A", "A")).unwrap();
        let result = TopologicalExecutor::new()
            .execute(&graph, &HandlerRegistry::new())
            .await
            .unwrap();
        assert_eq!(result.node_states["A"], NodeState::Completed);
    }

    // -- Wave grouping --

    #[test]
    fn wave_grouping_linear() {
        let graph = linear_graph();
        let waves = compute_waves(&graph).unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0].len(), 1);
        assert_eq!(waves[1].len(), 1);
        assert_eq!(waves[2].len(), 1);
    }

    #[test]
    fn wave_grouping_diamond() {
        let mut graph = Graph::new();
        for id in ["A", "B", "C", "D"] {
            graph.add_node(Node::new(id, id)).unwrap();
        }
        graph.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        graph.add_edge(&"A".into(), "", &"C".into(), "", None).unwrap();
        graph.add_edge(&"B".into(), "", &"D".into(), "", None).unwrap();
        graph.add_edge(&"C".into(), "", &"D".into(), "", None).unwrap();

        let waves = compute_waves(&graph).unwrap();
        assert_eq!(waves.len(), 3);
        assert_eq!(waves[0].len(), 1); // A
        assert_eq!(waves[1].len(), 2); // B, C (parallel)
        assert_eq!(waves[2].len(), 1); // D
    }

    // -- Data flow --

    #[tokio::test]
    async fn data_flows_through_named_ports() {
        let mut graph = Graph::new();
        graph
            .add_node(Node::new("SRC", "Source").with_handler("produce"))
            .unwrap();
        graph
            .add_node(Node::new("DST", "Dest").with_handler("consume"))
            .unwrap();
        graph
            .add_edge(&"SRC".into(), "value", &"DST".into(), "input", None)
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "produce".into(),
            sync_handler(|_, _| {
                let mut out = Outputs::new();
                out.insert("value".into(), Value::I64(42));
                Ok(out)
            }),
        );
        handlers.insert(
            "consume".into(),
            sync_handler(|_, inputs| {
                assert_eq!(inputs.get("input"), Some(&Value::I64(42)));
                Ok(inputs)
            }),
        );

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();
        assert_eq!(result.node_states["DST"], NodeState::Completed);
    }

    // -- Error propagation --

    #[tokio::test]
    async fn error_cascades_to_downstream() {
        let mut graph = Graph::new();
        graph.add_node(Node::new("A", "A").with_handler("ok")).unwrap();
        graph.add_node(Node::new("B", "B").with_handler("fail")).unwrap();
        graph.add_node(Node::new("C", "C").with_handler("ok")).unwrap();
        graph.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        graph.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();

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

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Failed);
        assert_eq!(result.node_states["C"], NodeState::Cancelled);
    }

    #[tokio::test]
    async fn concurrent_failures_both_cancel_downstream() {
        // A → (B, C) → D. Both B and C fail.
        let mut graph = Graph::new();
        graph.add_node(Node::new("A", "A").with_handler("ok")).unwrap();
        graph.add_node(Node::new("B", "B").with_handler("fail")).unwrap();
        graph.add_node(Node::new("C", "C").with_handler("fail")).unwrap();
        graph.add_node(Node::new("D", "D").with_handler("ok")).unwrap();
        graph.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        graph.add_edge(&"A".into(), "", &"C".into(), "", None).unwrap();
        graph.add_edge(&"B".into(), "", &"D".into(), "", None).unwrap();
        graph.add_edge(&"C".into(), "", &"D".into(), "", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("ok".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "fail".into(),
            sync_handler(|_, _| {
                Err(NodeError::Failed {
                    source_message: None,
                    message: "fail".into(),
                    recoverable: false,
                })
            }),
        );

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Failed);
        assert_eq!(result.node_states["C"], NodeState::Failed);
        assert_eq!(result.node_states["D"], NodeState::Cancelled);
    }

    // -- Cancellation --

    #[tokio::test]
    async fn global_cancellation_stops_execution() {
        let mut graph = Graph::new();
        graph
            .add_node(Node::new("A", "A").with_handler("cancel_it"))
            .unwrap();
        graph
            .add_node(Node::new("B", "B").with_handler("ok"))
            .unwrap();
        graph.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();

        let token = CancellationToken::new();
        let mut handlers = HandlerRegistry::new();
        let cancel_clone = token.clone();
        handlers.insert(
            "cancel_it".into(),
            sync_handler(move |_, inputs| {
                cancel_clone.cancel();
                Ok(inputs)
            }),
        );
        handlers.insert("ok".into(), sync_handler(|_, inputs| Ok(inputs)));

        let result = TopologicalExecutor::with_cancel(token)
            .execute(&graph, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Cancelled);
    }

    #[tokio::test]
    async fn in_flight_handler_checks_cancellation() {
        // Two independent nodes in the same wave.
        // CANCELLER cancels the token after 10ms.
        // WORKER checks cancellation after 50ms.
        let mut graph = Graph::new();
        graph
            .add_node(Node::new("CANCELLER", "C").with_handler("cancel_async"))
            .unwrap();
        graph
            .add_node(Node::new("WORKER", "W").with_handler("check_cancel"))
            .unwrap();
        // No edges — both in wave 0

        let token = CancellationToken::new();
        let mut handlers = HandlerRegistry::new();

        let t = token.clone();
        handlers.insert(
            "cancel_async".into(),
            Arc::new(AsyncCancelHandler(t)) as Arc<dyn NodeHandler>,
        );
        handlers.insert(
            "check_cancel".into(),
            Arc::new(CancelCheckHandler) as Arc<dyn NodeHandler>,
        );

        let result = TopologicalExecutor::with_cancel(token)
            .execute(&graph, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["CANCELLER"], NodeState::Completed);
        assert_eq!(result.node_states["WORKER"], NodeState::Cancelled);
    }

    // -- Timeouts --

    #[tokio::test]
    async fn per_node_timeout() {
        let mut graph = Graph::new();
        let mut slow = Node::new("SLOW", "Slow").with_handler("slow");
        slow.exec = serde_json::json!({ "timeout_ms": 50 });
        graph.add_node(slow).unwrap();
        graph
            .add_node(Node::new("AFTER", "After").with_handler("ok"))
            .unwrap();
        graph
            .add_edge(&"SLOW".into(), "", &"AFTER".into(), "", None)
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "slow".into(),
            Arc::new(SlowHandler(Duration::from_millis(500))),
        );
        handlers.insert("ok".into(), sync_handler(|_, inputs| Ok(inputs)));

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["SLOW"], NodeState::Failed);
        assert_eq!(result.node_states["AFTER"], NodeState::Cancelled);
    }

    // -- Events --

    #[tokio::test]
    async fn events_emitted_during_execution() {
        let graph = linear_graph();
        let result = TopologicalExecutor::new()
            .execute(&graph, &trace_handlers())
            .await
            .unwrap();

        let state_changes = result
            .events
            .iter()
            .filter(|e| matches!(e, ExecutionEvent::StateChanged { .. }))
            .count();
        // Each node: Idle→Pending, Pending→Running, Running→Completed = 3 × 3 nodes = 9
        assert!(
            state_changes >= 9,
            "expected at least 9 state changes, got {state_changes}"
        );

        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, ExecutionEvent::ExecutionStarted { .. })));
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, ExecutionEvent::ExecutionCompleted { .. })));
    }

    // -- Trait conformance --

    #[tokio::test]
    async fn executor_trait_conformance() {
        let executor: Box<dyn Executor> = Box::new(TopologicalExecutor::new());

        // Single node
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        let result = executor
            .execute(&g, &HandlerRegistry::new())
            .await
            .unwrap();
        assert_eq!(result.node_states["A"], NodeState::Completed);

        // Linear chain with error propagation
        let mut g = Graph::new();
        g.add_node(Node::new("X", "X").with_handler("ok")).unwrap();
        g.add_node(Node::new("Y", "Y").with_handler("fail")).unwrap();
        g.add_node(Node::new("Z", "Z").with_handler("ok")).unwrap();
        g.add_edge(&"X".into(), "", &"Y".into(), "", None).unwrap();
        g.add_edge(&"Y".into(), "", &"Z".into(), "", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("ok".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "fail".into(),
            sync_handler(|_, _| {
                Err(NodeError::Failed {
                    source_message: None,
                    message: "fail".into(),
                    recoverable: false,
                })
            }),
        );

        let result = executor.execute(&g, &handlers).await.unwrap();
        assert_eq!(result.node_states["X"], NodeState::Completed);
        assert_eq!(result.node_states["Y"], NodeState::Failed);
        assert_eq!(result.node_states["Z"], NodeState::Cancelled);
    }

    // -- Test helpers --

    struct SlowHandler(Duration);

    impl NodeHandler for SlowHandler {
        fn execute(
            &self,
            _node: &Node,
            inputs: Outputs,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
            let dur = self.0;
            Box::pin(async move {
                tokio::time::sleep(dur).await;
                Ok(inputs)
            })
        }
    }

    struct AsyncCancelHandler(CancellationToken);

    impl NodeHandler for AsyncCancelHandler {
        fn execute(
            &self,
            _node: &Node,
            inputs: Outputs,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
            let token = self.0.clone();
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                token.cancel();
                Ok(inputs)
            })
        }
    }

    struct CancelCheckHandler;

    impl NodeHandler for CancelCheckHandler {
        fn execute(
            &self,
            _node: &Node,
            _inputs: Outputs,
            cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if cancel.is_cancelled() {
                    return Err(NodeError::Cancelled {
                        reason: "cancelled mid-execution".into(),
                    });
                }
                Ok(Outputs::new())
            })
        }
    }
}
