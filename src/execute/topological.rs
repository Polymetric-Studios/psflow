use crate::error::NodeError;
use crate::execute::context::{CancellationToken, ExecutionContext};
use crate::execute::control;
use crate::execute::event::ExecutionEvent;
use crate::execute::lifecycle::NodeState;
use crate::execute::concurrency::ConcurrencyLimits;
use crate::execute::{
    ExecutionError, ExecutionResult, Executor, HandlerRegistry, NodeHandler, Outputs,
};
use crate::graph::node::NodeId;
use crate::graph::{Graph, SubgraphDirective};
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
    concurrency: ConcurrencyLimits,
}

impl TopologicalExecutor {
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

    /// Set global concurrency limits for this executor.
    pub fn with_concurrency(mut self, limits: ConcurrencyLimits) -> Self {
        self.concurrency = limits;
        self
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }

    /// Execute a graph with a child blackboard that inherits from a parent.
    pub async fn execute_with_parent(
        &self,
        graph: &Graph,
        handlers: &HandlerRegistry,
        parent_bb: &crate::execute::blackboard::Blackboard,
        inheritance: crate::execute::blackboard::ContextInheritance,
    ) -> Result<ExecutionResult, ExecutionError> {
        execute_impl_with_blackboard(
            graph,
            handlers,
            self.cancel_token.clone(),
            self.concurrency.clone(),
            Some((parent_bb, inheritance)),
        )
        .await
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
        Box::pin(execute_impl_with_blackboard(
            graph,
            handlers,
            self.cancel_token.clone(),
            self.concurrency.clone(),
            None,
        ))
    }
}

async fn execute_impl_with_blackboard(
    graph: &Graph,
    handlers: &HandlerRegistry,
    cancel_token: CancellationToken,
    concurrency: ConcurrencyLimits,
    parent: Option<(
        &crate::execute::blackboard::Blackboard,
        crate::execute::blackboard::ContextInheritance,
    )>,
) -> Result<ExecutionResult, ExecutionError> {
    let start = Instant::now();
    let ctx = if let Some((parent_bb, inheritance)) = parent {
        Arc::new(ExecutionContext::with_parent_blackboard(
            cancel_token,
            parent_bb,
            inheritance,
            concurrency,
        ))
    } else {
        Arc::new(ExecutionContext::with_concurrency(cancel_token, concurrency))
    };

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

    // Build subgraph membership: node_id → subgraph index
    let subgraph_membership = build_subgraph_membership(graph);
    // Track which subgraphs have been fully executed
    let mut executed_subgraphs: HashSet<String> = HashSet::new();

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

        // Separate nodes into subgraph-managed and independently-executed
        let mut subgraphs_to_run: Vec<usize> = Vec::new();
        let mut independent_nodes: Vec<&NodeId> = Vec::new();

        for node_id in wave {
            if ctx.get_state(&node_id.0).is_terminal() {
                continue;
            }
            if let Some(sg_idx) = subgraph_membership.get(node_id) {
                let sg = &graph.subgraphs()[*sg_idx];
                if sg.directive != SubgraphDirective::None
                    && !executed_subgraphs.contains(&sg.id)
                    && !subgraphs_to_run.contains(sg_idx)
                {
                    subgraphs_to_run.push(*sg_idx);
                }
            } else {
                independent_nodes.push(node_id);
            }
        }

        // Execute subgraphs with their directives
        for sg_idx in &subgraphs_to_run {
            let sg = &graph.subgraphs()[*sg_idx];
            executed_subgraphs.insert(sg.id.clone());

            // Filter to non-terminal nodes in the subgraph
            let sg_nodes: Vec<NodeId> = sg
                .nodes
                .iter()
                .filter(|id| !ctx.get_state(&id.0).is_terminal())
                .cloned()
                .collect();

            if sg_nodes.is_empty() {
                continue;
            }

            match &sg.directive {
                SubgraphDirective::Parallel => {
                    let max_concurrent = parse_max_concurrent(graph, &sg.nodes);
                    control::execute_parallel(
                        &sg_nodes, graph, handlers, &ctx, &passthrough, max_concurrent,
                    )
                    .await?;
                }
                SubgraphDirective::Race => {
                    control::execute_race(
                        &sg_nodes, graph, handlers, &ctx, &passthrough,
                    )
                    .await?;
                }
                SubgraphDirective::Loop => {
                    let loop_config = parse_loop_config(graph, &sg.nodes);
                    control::execute_loop(
                        &sg_nodes, &loop_config, graph, handlers, &ctx, &passthrough,
                    )
                    .await?;
                }
                SubgraphDirective::Event => {
                    // Event subgraphs require the EventDrivenExecutor
                    return Err(ExecutionError::ValidationFailed(
                        format!("subgraph '{}' uses Event directive — use EventDrivenExecutor instead", sg.id)
                    ));
                }
                _ => {
                    // Named or None with nodes — execute as sequence
                    control::execute_sequence(
                        &sg_nodes, graph, handlers, &ctx, &passthrough, true,
                    )
                    .await?;
                }
            }
        }

        // Execute independent nodes (original wave-based parallel logic)
        let mut handles = Vec::new();

        for node_id in &independent_nodes {
            if ctx.get_state(&node_id.0).is_terminal() {
                continue;
            }

            // Check if this node is blocked by an upstream branch decision
            if is_branch_blocked(graph, node_id, &ctx) {
                let _ = ctx.set_state(&node_id.0, NodeState::Cancelled);
                cancel_downstream(graph, node_id, &ctx);
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

            let timeout_dur = node
                .exec
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .map(Duration::from_millis);

            let retry_config = crate::execute::retry::RetryConfig::from_exec(&node.exec);

            // Acquire global concurrency permit (blocks if at limit)
            let _global_permit = ctx.concurrency().acquire().await;

            ctx.set_state(&node_id_str, NodeState::Pending)
                .map_err(|e| ExecutionError::ValidationFailed(e.to_string()))?;

            handles.push(tokio::spawn(async move {
                // Move permit into task so it's held until completion
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

        // Await all tasks in this wave
        for handle in handles {
            let (node_id, outcome) = handle
                .await
                .map_err(|e| ExecutionError::ValidationFailed(format!("task panic: {e}")))?;

            match outcome {
                Ok(outputs) => {
                    // Check if this is a branch node and record the decision
                    handle_branch_decision(graph, &node_id, &outputs, &ctx);

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

/// Build a map from node ID to the subgraph index it belongs to.
fn build_subgraph_membership(graph: &Graph) -> HashMap<NodeId, usize> {
    let mut membership = HashMap::new();
    for (idx, sg) in graph.subgraphs().iter().enumerate() {
        for node_id in &sg.nodes {
            membership.insert(node_id.clone(), idx);
        }
    }
    membership
}

/// Check if a node is a branch node (has a guard config) and record the decision.
pub(crate) fn handle_branch_decision(
    graph: &Graph,
    node_id: &str,
    outputs: &Outputs,
    ctx: &ExecutionContext,
) {
    let nid = NodeId::new(node_id);
    let Some(node) = graph.node(&nid) else {
        return;
    };

    // A node is a branch if it has config.guard
    let guard_expr = node.config.get("guard").and_then(|v| v.as_str());
    let Some(guard_expr) = guard_expr else {
        return;
    };

    let bb = ctx.blackboard();
    let result = control::evaluate_guard(guard_expr, outputs, &bb);
    drop(bb);

    match result {
        Ok(guard_result) => {
            ctx.set_branch_decision(node_id, guard_result.edge_label().to_string());
        }
        Err(_) => {
            // Guard evaluation failed — default to "no"
            ctx.set_branch_decision(node_id, "no".to_string());
        }
    }
}

/// Check if a node is blocked because an upstream branch didn't select its incoming edge.
pub(crate) fn is_branch_blocked(graph: &Graph, node_id: &NodeId, ctx: &ExecutionContext) -> bool {
    for (src_node, edge_data) in graph.incoming_edges(node_id) {
        if let Some(decision) = ctx.get_branch_decision(&src_node.id.0) {
            // The upstream node made a branch decision.
            // If this edge has a label, it must match the decision.
            if let Some(ref label) = edge_data.label {
                if label != &decision {
                    return true;
                }
            }
        }
    }
    false
}

/// Parse loop configuration from the first node in the subgraph that has exec.loop settings.
/// Parse `exec.max_concurrent` from subgraph nodes.
fn parse_max_concurrent(graph: &Graph, node_ids: &[NodeId]) -> Option<usize> {
    for nid in node_ids {
        if let Some(node) = graph.node(nid) {
            if let Some(max) = node.exec.get("max_concurrent").and_then(|v| v.as_u64()) {
                return Some(max as usize);
            }
        }
    }
    None
}

fn parse_loop_config(graph: &Graph, node_ids: &[NodeId]) -> control::LoopConfig {
    for nid in node_ids {
        if let Some(node) = graph.node(nid) {
            if let Some(count) = node.exec.get("loop_count").and_then(|v| v.as_u64()) {
                return control::LoopConfig::Repeat(count as usize);
            }
            if let Some(guard) = node.exec.get("loop_while").and_then(|v| v.as_str()) {
                let max = node
                    .exec
                    .get("loop_max_iterations")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(control::DEFAULT_MAX_LOOP_ITERATIONS);
                return control::LoopConfig::While {
                    guard: guard.to_string(),
                    max_iterations: max,
                };
            }
        }
    }
    // Default: single execution (same as sequence)
    control::LoopConfig::Repeat(1)
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
pub(crate) fn collect_inputs(graph: &Graph, node_id: &NodeId, ctx: &ExecutionContext) -> Outputs {
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
pub(crate) fn cancel_downstream(graph: &Graph, failed_id: &NodeId, ctx: &ExecutionContext) {
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
pub(crate) struct PassthroughHandler;

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

    // -- Branch / conditional tests --

    #[tokio::test]
    async fn branch_follows_yes_path() {
        // BRANCH(guard: "inputs.flag == true") --yes--> YES_NODE
        //                                      --no---> NO_NODE
        let mut graph = Graph::new();

        let producer = Node::new("PROD", "Producer").with_handler("produce_true");
        graph.add_node(producer).unwrap();

        let mut branch = Node::new("BR", "Branch").with_handler("pass_through");
        branch.config = serde_json::json!({ "guard": "inputs.flag == true" });
        graph.add_node(branch).unwrap();

        graph.add_node(Node::new("YES", "Yes").with_handler("ok")).unwrap();
        graph.add_node(Node::new("NO", "No").with_handler("ok")).unwrap();

        graph.add_edge(&"PROD".into(), "flag", &"BR".into(), "flag", None).unwrap();
        graph.add_edge(&"BR".into(), "", &"YES".into(), "", Some("yes".into())).unwrap();
        graph.add_edge(&"BR".into(), "", &"NO".into(), "", Some("no".into())).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("ok".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "produce_true".into(),
            sync_handler(|_, _| {
                let mut out = Outputs::new();
                out.insert("flag".into(), Value::Bool(true));
                Ok(out)
            }),
        );
        handlers.insert(
            "pass_through".into(),
            sync_handler(|_, inputs| Ok(inputs)),
        );

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["BR"], NodeState::Completed);
        assert_eq!(result.node_states["YES"], NodeState::Completed);
        assert_eq!(result.node_states["NO"], NodeState::Cancelled);
    }

    #[tokio::test]
    async fn branch_follows_no_path() {
        let mut graph = Graph::new();

        graph.add_node(Node::new("PROD", "Producer").with_handler("produce_false")).unwrap();

        let mut branch = Node::new("BR", "Branch").with_handler("pass_through");
        branch.config = serde_json::json!({ "guard": "inputs.flag == true" });
        graph.add_node(branch).unwrap();

        graph.add_node(Node::new("YES", "Yes").with_handler("ok")).unwrap();
        graph.add_node(Node::new("NO", "No").with_handler("ok")).unwrap();

        graph.add_edge(&"PROD".into(), "flag", &"BR".into(), "flag", None).unwrap();
        graph.add_edge(&"BR".into(), "", &"YES".into(), "", Some("yes".into())).unwrap();
        graph.add_edge(&"BR".into(), "", &"NO".into(), "", Some("no".into())).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("ok".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "produce_false".into(),
            sync_handler(|_, _| {
                let mut out = Outputs::new();
                out.insert("flag".into(), Value::Bool(false));
                Ok(out)
            }),
        );
        handlers.insert(
            "pass_through".into(),
            sync_handler(|_, inputs| Ok(inputs)),
        );

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["BR"], NodeState::Completed);
        assert_eq!(result.node_states["YES"], NodeState::Cancelled);
        assert_eq!(result.node_states["NO"], NodeState::Completed);
    }

    // -- Subgraph directive tests --

    #[tokio::test]
    async fn parallel_subgraph_directive() {
        // Two independent nodes in a parallel subgraph
        let mut graph = Graph::new();
        graph.add_node(Node::new("A", "A").with_handler("trace")).unwrap();
        graph.add_node(Node::new("B", "B").with_handler("trace")).unwrap();

        graph.add_subgraph(crate::graph::Subgraph {
            id: "sg1".into(),
            label: "parallel: workers".into(),
            directive: SubgraphDirective::Parallel,
            nodes: vec!["A".into(), "B".into()],
            children: Vec::new(),
        });

        let result = TopologicalExecutor::new()
            .execute(&graph, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Completed);
    }

    #[tokio::test]
    async fn race_subgraph_one_wins() {
        // Two nodes in a race subgraph. Both complete, but the framework
        // should cancel siblings when the first completes.
        let mut graph = Graph::new();
        graph.add_node(Node::new("FAST", "Fast").with_handler("fast")).unwrap();
        graph.add_node(Node::new("SLOW", "Slow").with_handler("slow")).unwrap();

        graph.add_subgraph(crate::graph::Subgraph {
            id: "race1".into(),
            label: "race: candidates".into(),
            directive: SubgraphDirective::Race,
            nodes: vec!["FAST".into(), "SLOW".into()],
            children: Vec::new(),
        });

        let mut handlers = HandlerRegistry::new();
        handlers.insert("fast".into(), sync_handler(|_, _| {
            let mut out = Outputs::new();
            out.insert("result".into(), Value::String("fast_won".into()));
            Ok(out)
        }));
        handlers.insert(
            "slow".into(),
            Arc::new(SlowHandler(Duration::from_millis(200))),
        );

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        // At least one completed, and siblings should be cancelled
        let fast_state = result.node_states["FAST"];
        let slow_state = result.node_states["SLOW"];
        assert!(
            (fast_state == NodeState::Completed && slow_state == NodeState::Cancelled)
                || (fast_state == NodeState::Cancelled && slow_state == NodeState::Completed),
            "expected one winner and one cancelled, got fast={fast_state:?} slow={slow_state:?}"
        );
    }

    #[tokio::test]
    async fn loop_subgraph_repeats() {
        // A node in a loop subgraph with loop_count: 3
        // The node increments a counter each iteration
        use std::sync::atomic::{AtomicUsize, Ordering};

        let counter = Arc::new(AtomicUsize::new(0));

        let mut node = Node::new("COUNTER", "Counter").with_handler("count");
        node.exec = serde_json::json!({ "loop_count": 3 });
        let mut graph = Graph::new();
        graph.add_node(node).unwrap();

        graph.add_subgraph(crate::graph::Subgraph {
            id: "loop1".into(),
            label: "loop: repeat".into(),
            directive: SubgraphDirective::Loop,
            nodes: vec!["COUNTER".into()],
            children: Vec::new(),
        });

        let counter_clone = counter.clone();
        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "count".into(),
            sync_handler(move |_, _| {
                counter_clone.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(Outputs::new())
            }),
        );

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        assert_eq!(counter.load(AtomicOrdering::SeqCst), 3);
        assert_eq!(result.node_states["COUNTER"], NodeState::Completed);
    }

    #[tokio::test]
    async fn sequence_in_named_subgraph() {
        // Nodes in a Named subgraph execute as a sequence
        let mut graph = Graph::new();
        graph.add_node(Node::new("S1", "Step 1").with_handler("trace")).unwrap();
        graph.add_node(Node::new("S2", "Step 2").with_handler("trace")).unwrap();
        graph.add_node(Node::new("S3", "Step 3").with_handler("trace")).unwrap();
        graph.add_edge(&"S1".into(), "", &"S2".into(), "", None).unwrap();
        graph.add_edge(&"S2".into(), "", &"S3".into(), "", None).unwrap();

        graph.add_subgraph(crate::graph::Subgraph {
            id: "seq1".into(),
            label: "pipeline".into(),
            directive: SubgraphDirective::Named("pipeline".into()),
            nodes: vec!["S1".into(), "S2".into(), "S3".into()],
            children: Vec::new(),
        });

        let result = TopologicalExecutor::new()
            .execute(&graph, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states["S1"], NodeState::Completed);
        assert_eq!(result.node_states["S2"], NodeState::Completed);
        assert_eq!(result.node_states["S3"], NodeState::Completed);
    }

    #[tokio::test]
    async fn sequence_fail_fast_cancels_remaining() {
        // S1 → S2(fail) → S3. S3 should be cancelled.
        let mut graph = Graph::new();
        graph.add_node(Node::new("S1", "S1").with_handler("ok")).unwrap();
        graph.add_node(Node::new("S2", "S2").with_handler("fail")).unwrap();
        graph.add_node(Node::new("S3", "S3").with_handler("ok")).unwrap();
        graph.add_edge(&"S1".into(), "", &"S2".into(), "", None).unwrap();
        graph.add_edge(&"S2".into(), "", &"S3".into(), "", None).unwrap();

        graph.add_subgraph(crate::graph::Subgraph {
            id: "seq1".into(),
            label: "pipeline".into(),
            directive: SubgraphDirective::Named("pipeline".into()),
            nodes: vec!["S1".into(), "S2".into(), "S3".into()],
            children: Vec::new(),
        });

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

        assert_eq!(result.node_states["S1"], NodeState::Completed);
        assert_eq!(result.node_states["S2"], NodeState::Failed);
        assert_eq!(result.node_states["S3"], NodeState::Cancelled);
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

    // -- Retry integration tests --

    #[tokio::test]
    async fn node_with_retry_succeeds_on_second_attempt() {
        use std::sync::atomic::AtomicU32;

        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let mut graph = Graph::new();
        let mut node = Node::new("R", "Retry").with_handler("flaky");
        node.exec = serde_json::json!({
            "retry": { "max_attempts": 3, "delay_ms": 1 }
        });
        graph.add_node(node).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "flaky".into(),
            sync_handler(move |_, _| {
                let n = counter_clone.fetch_add(1, AtomicOrdering::SeqCst);
                if n == 0 {
                    Err(NodeError::Failed {
                        source_message: None,
                        message: "transient".into(),
                        recoverable: true,
                    })
                } else {
                    Ok(Outputs::new())
                }
            }),
        );

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["R"], NodeState::Completed);
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn node_without_retry_fails_immediately() {
        let mut graph = Graph::new();
        graph
            .add_node(Node::new("F", "Fail").with_handler("fail"))
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "fail".into(),
            sync_handler(|_, _| {
                Err(NodeError::Failed {
                    source_message: None,
                    message: "fatal".into(),
                    recoverable: true,
                })
            }),
        );

        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["F"], NodeState::Failed);
    }

    // -- Concurrency limit integration tests --

    #[tokio::test]
    async fn concurrency_limit_respected() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        // 5 independent nodes with max_parallelism: 2
        let mut graph = Graph::new();
        for i in 0..5 {
            graph
                .add_node(Node::new(format!("N{i}"), format!("N{i}")).with_handler("track"))
                .unwrap();
        }

        let c = concurrent.clone();
        let m = max_seen.clone();
        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "track".into(),
            Arc::new(ConcurrencyTracker(c, m)) as Arc<dyn NodeHandler>,
        );

        let result = TopologicalExecutor::new()
            .with_concurrency(
                crate::execute::concurrency::ConcurrencyLimits::with_max_parallelism(2),
            )
            .execute(&graph, &handlers)
            .await
            .unwrap();

        for i in 0..5 {
            assert_eq!(
                result.node_states[&format!("N{i}")],
                NodeState::Completed
            );
        }
        assert!(
            max_seen.load(AtomicOrdering::SeqCst) <= 2,
            "max concurrent was {}, expected <= 2",
            max_seen.load(AtomicOrdering::SeqCst)
        );
    }

    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

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
                // Update max seen
                max_seen.fetch_max(cur, AtomicOrdering::SeqCst);
                tokio::time::sleep(Duration::from_millis(20)).await;
                concurrent.fetch_sub(1, AtomicOrdering::SeqCst);
                Ok(Outputs::new())
            })
        }
    }

    #[tokio::test]
    async fn no_concurrency_limit_allows_full_parallelism() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let max_seen = Arc::new(AtomicUsize::new(0));
        let concurrent = Arc::new(AtomicUsize::new(0));

        let mut graph = Graph::new();
        for i in 0..4 {
            graph
                .add_node(Node::new(format!("N{i}"), format!("N{i}")).with_handler("track"))
                .unwrap();
        }

        let c = concurrent.clone();
        let m = max_seen.clone();
        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "track".into(),
            Arc::new(ConcurrencyTracker(c, m)) as Arc<dyn NodeHandler>,
        );

        // No concurrency limit
        let result = TopologicalExecutor::new()
            .execute(&graph, &handlers)
            .await
            .unwrap();

        for i in 0..4 {
            assert_eq!(result.node_states[&format!("N{i}")], NodeState::Completed);
        }
        // All 4 should run concurrently (same wave, no deps)
        assert!(
            max_seen.load(AtomicOrdering::SeqCst) >= 3,
            "expected at least 3 concurrent, got {}",
            max_seen.load(AtomicOrdering::SeqCst)
        );
    }
}
