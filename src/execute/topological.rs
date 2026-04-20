use crate::error::NodeError;
use crate::execute::concurrency::ConcurrencyLimits;
use crate::execute::context::{CancellationToken, ExecutionContext};
use crate::execute::control;
use crate::execute::event::ExecutionEvent;
use crate::execute::lifecycle::NodeState;
use crate::execute::{
    ExecutionError, ExecutionResult, Executor, HandlerRegistry, NodeHandler, Outputs,
};
use crate::graph::node::NodeId;
use crate::graph::{Graph, SubgraphDirective};
use petgraph::algo::toposort;
use petgraph::stable_graph::NodeIndex;
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, info_span, trace, warn, Instrument};

/// Executes graphs in dependency order, running independent nodes in parallel waves.
pub struct TopologicalExecutor {
    cancel_token: CancellationToken,
    concurrency: ConcurrencyLimits,
    adapter: Option<Arc<dyn crate::adapter::AiAdapter>>,
}

impl TopologicalExecutor {
    pub fn new() -> Self {
        Self {
            cancel_token: CancellationToken::new(),
            concurrency: ConcurrencyLimits::new(),
            adapter: None,
        }
    }

    pub fn with_cancel(token: CancellationToken) -> Self {
        Self {
            cancel_token: token,
            concurrency: ConcurrencyLimits::new(),
            adapter: None,
        }
    }

    /// Set global concurrency limits for this executor.
    pub fn with_concurrency(mut self, limits: ConcurrencyLimits) -> Self {
        self.concurrency = limits;
        self
    }

    /// Set an AI adapter for LLM oracle evaluations (guard_llm, criterion_llm, while_llm).
    pub fn with_adapter(mut self, adapter: Arc<dyn crate::adapter::AiAdapter>) -> Self {
        self.adapter = Some(adapter);
        self
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }

    /// Resume execution from a snapshot.
    ///
    /// Completed nodes are skipped (their state and outputs are preserved).
    /// Interrupted (Running/Pending) nodes are reset to Idle and re-executed.
    /// The blackboard is restored from the snapshot.
    /// The executor's cancel token and concurrency limits are preserved.
    pub async fn resume(
        &self,
        graph: &Graph,
        handlers: &HandlerRegistry,
        snapshot: crate::execute::snapshot::ExecutionSnapshot,
    ) -> Result<ExecutionResult, ExecutionError> {
        let ctx = ExecutionContext::from_snapshot_with(
            snapshot,
            self.cancel_token.clone(),
            self.concurrency.clone(),
        );
        execute_impl_from_context(graph, handlers, Arc::new(ctx), self.adapter.clone()).await
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
            self.adapter.clone(),
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
            self.adapter.clone(),
        ))
    }
}

/// Execute with a pre-built context (used for snapshot resume).
async fn execute_impl_from_context(
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: Arc<ExecutionContext>,
    adapter: Option<Arc<dyn crate::adapter::AiAdapter>>,
) -> Result<ExecutionResult, ExecutionError> {
    let graph_name = graph.metadata().name.as_deref().unwrap_or("unnamed");

    let start = Instant::now();
    info!(
        graph = graph_name,
        nodes = graph.node_count(),
        "execution resumed"
    );

    ctx.emit(ExecutionEvent::ExecutionStarted { timestamp: start });

    execute_core(graph, handlers, ctx, adapter, start).await
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
    adapter: Option<Arc<dyn crate::adapter::AiAdapter>>,
) -> Result<ExecutionResult, ExecutionError> {
    let graph_name = graph.metadata().name.as_deref().unwrap_or("unnamed");

    let start = Instant::now();
    info!(
        graph = graph_name,
        nodes = graph.node_count(),
        edges = graph.edge_count(),
        "execution started"
    );

    let ctx = if let Some((parent_bb, inheritance)) = parent {
        Arc::new(ExecutionContext::with_parent_blackboard(
            cancel_token,
            parent_bb,
            inheritance,
            concurrency,
        ))
    } else {
        Arc::new(ExecutionContext::with_concurrency(
            cancel_token,
            concurrency,
        ))
    };

    ctx.emit(ExecutionEvent::ExecutionStarted { timestamp: start });

    execute_core(graph, handlers, ctx, adapter, start).await
}

/// Core execution logic shared by fresh execution and snapshot resume.
async fn execute_core(
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: Arc<ExecutionContext>,
    adapter: Option<Arc<dyn crate::adapter::AiAdapter>>,
    start: Instant,
) -> Result<ExecutionResult, ExecutionError> {
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
    debug!(wave_count = waves.len(), "dependency waves computed");
    let passthrough: Arc<dyn NodeHandler> = Arc::new(PassthroughHandler);

    for (wave_idx, wave) in waves.iter().enumerate() {
        debug!(wave = wave_idx, nodes = wave.len(), "processing wave");
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
            let state = ctx.get_state(&node_id.0);
            if state.is_terminal() || state.is_suspended() {
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

            // Filter to non-terminal and non-suspended nodes in the subgraph
            let sg_nodes: Vec<NodeId> = sg
                .nodes
                .iter()
                .filter(|id| {
                    let s = ctx.get_state(&id.0);
                    !s.is_terminal() && !s.is_suspended()
                })
                .cloned()
                .collect();

            if sg_nodes.is_empty() {
                continue;
            }

            match &sg.directive {
                SubgraphDirective::Parallel => {
                    let max_concurrent = parse_max_concurrent(graph, &sg.nodes);
                    control::execute_parallel(
                        &sg_nodes,
                        graph,
                        handlers,
                        &ctx,
                        &passthrough,
                        max_concurrent,
                    )
                    .await?;
                }
                SubgraphDirective::Race => {
                    control::execute_race_with_adapter(
                        &sg_nodes,
                        graph,
                        handlers,
                        &ctx,
                        &passthrough,
                        adapter.as_deref(),
                    )
                    .await?;
                }
                SubgraphDirective::Loop => {
                    let loop_config = parse_loop_config(graph, &sg.nodes);
                    control::execute_loop_with_adapter(
                        &sg_nodes,
                        &loop_config,
                        graph,
                        handlers,
                        &ctx,
                        &passthrough,
                        adapter.as_deref(),
                    )
                    .await?;
                }
                SubgraphDirective::Event => {
                    // In topological mode, treat event subgraphs as a sequence.
                    // The trigger nodes passthrough — the graph continues from
                    // whichever node they feed into.
                    debug!(subgraph = %sg.id, "event subgraph — running as sequence in CLI mode");
                    control::execute_sequence(&sg_nodes, graph, handlers, &ctx, &passthrough, true)
                        .await?;
                }
                _ => {
                    // Named or None with nodes — execute as sequence
                    control::execute_sequence(&sg_nodes, graph, handlers, &ctx, &passthrough, true)
                        .await?;
                }
            }
        }

        // Execute independent nodes (original wave-based parallel logic)
        let mut handles = Vec::new();

        for node_id in &independent_nodes {
            let ind_state = ctx.get_state(&node_id.0);
            if ind_state.is_terminal() || ind_state.is_suspended() {
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

            let handler_name = node.handler.clone().unwrap_or_else(|| "passthrough".into());
            let node_span = info_span!("node", id = %node_id_str, handler = %handler_name);

            handles.push(tokio::spawn(
                async move {
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

                    trace!(node = %node_id_str, "handler executing");

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
                }
                .instrument(node_span),
            ));
        }

        // Await all tasks in this wave
        for handle in handles {
            let (node_id, outcome) = handle
                .await
                .map_err(|e| ExecutionError::ValidationFailed(format!("task panic: {e}")))?;

            match outcome {
                Ok(outputs) => {
                    // Check if this is a branch node and record the decision
                    handle_branch_decision(graph, &node_id, &outputs, &ctx, adapter.as_deref())
                        .await;

                    debug!(node = %node_id, output_keys = ?outputs.keys().collect::<Vec<_>>(), "node completed");
                    ctx.store_outputs(&node_id, outputs.clone());
                    ctx.emit(ExecutionEvent::NodeCompleted {
                        node_id: node_id.clone(),
                        outputs,
                    });
                    let _ = ctx.set_state(&node_id, NodeState::Completed);
                }
                Err(NodeError::Cancelled { .. }) => {
                    debug!(node = %node_id, "node cancelled");
                    let _ = ctx.set_state(&node_id, NodeState::Cancelled);
                }
                Err(NodeError::Suspended { .. }) => {
                    debug!(node = %node_id, "node suspended (awaiting external result)");
                    let _ = ctx.set_state(&node_id, NodeState::Suspended);
                }
                Err(ref error) => {
                    warn!(node = %node_id, error = %error, "node failed");
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
    info!(
        elapsed_ms = elapsed.as_millis() as u64,
        "execution completed"
    );
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
///
/// Supports two modes:
/// - `config.guard`: deterministic guard expression (evaluated synchronously)
/// - `config.guard_llm.prompt`: LLM-based guard (async adapter call, with fallback)
pub(crate) async fn handle_branch_decision(
    graph: &Graph,
    node_id: &str,
    outputs: &Outputs,
    ctx: &ExecutionContext,
    adapter: Option<&dyn crate::adapter::AiAdapter>,
) {
    let nid = NodeId::new(node_id);
    let Some(node) = graph.node(&nid) else {
        return;
    };

    // Check for LLM guard first
    let guard_llm = node.config.get("guard_llm");
    let guard_expr = node.config.get("guard").and_then(|v| v.as_str());

    // Neither guard nor guard_llm — not a branch node
    if guard_llm.is_none() && guard_expr.is_none() {
        return;
    }

    // Try LLM guard if configured and adapter available
    if let Some(llm_config) = guard_llm {
        if let Some(adapter) = adapter {
            let result = control::evaluate_guard_llm(llm_config, outputs, ctx, adapter).await;
            match result {
                Ok(label) => {
                    ctx.set_branch_decision(node_id, label);
                    return;
                }
                Err(_) => {
                    // Fall through to deterministic guard
                }
            }
        }
        // No adapter or adapter call failed — try deterministic fallback
        if let Some(fallback) = llm_config.get("fallback").and_then(|v| v.as_str()) {
            let bb = ctx.blackboard();
            let result = control::evaluate_guard(fallback, outputs, &bb);
            drop(bb);
            if let Ok(guard_result) = result {
                ctx.set_branch_decision(node_id, guard_result.edge_label().to_string());
                return;
            }
        }
    }

    // Deterministic guard
    if let Some(guard_expr) = guard_expr {
        let bb = ctx.blackboard();
        let result = control::evaluate_guard(guard_expr, outputs, &bb);
        drop(bb);

        match result {
            Ok(guard_result) => {
                ctx.set_branch_decision(node_id, guard_result.edge_label().to_string());
            }
            Err(_) => {
                ctx.set_branch_decision(node_id, "no".to_string());
            }
        }
    } else {
        // guard_llm with no deterministic fallback and adapter failed — default to "no"
        ctx.set_branch_decision(node_id, "no".to_string());
    }
}

/// Check if a node is blocked because an upstream branch didn't select its incoming edge.
/// Check if a node is blocked by a branch decision on an upstream node.
///
/// Edge label semantics:
/// - A labeled edge is blocked when its label doesn't match the branch decision.
/// - `"else"` is a fallback label: it's blocked only when another edge from the
///   same source carries the matching label. If no edge matches the decision,
///   the `"else"` edge becomes the active path.
/// - Unlabeled edges are never blocked by branch decisions.
pub(crate) fn is_branch_blocked(graph: &Graph, node_id: &NodeId, ctx: &ExecutionContext) -> bool {
    for (src_node, edge_data) in graph.incoming_edges(node_id) {
        if let Some(decision) = ctx.get_branch_decision(&src_node.id.0) {
            if let Some(ref label) = edge_data.label {
                if label == "else" {
                    // "else" is the fallback — blocked only if another edge
                    // from the same source has the matching label
                    let has_match = graph
                        .outgoing_edges(&src_node.id)
                        .iter()
                        .any(|(e, _)| e.label.as_deref() == Some(&decision));
                    if has_match {
                        return true;
                    }
                } else if label != &decision {
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
            // LLM-based loop condition
            if let Some(llm_config) = node.exec.get("while_llm") {
                let max = node
                    .exec
                    .get("loop_max_iterations")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(control::DEFAULT_MAX_LOOP_ITERATIONS);
                return control::LoopConfig::WhileLlm {
                    llm_config: llm_config.clone(),
                    max_iterations: max,
                };
            }
            // ForEach: iterate over a blackboard collection
            // Accepts both `loop_foreach` and `loop_over` (legacy alias from examples)
            if let Some(collection) = node
                .exec
                .get("loop_foreach")
                .or_else(|| node.exec.get("loop_over"))
                .and_then(|v| v.as_str())
            {
                let item_key = node
                    .exec
                    .get("loop_item_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("loop.item")
                    .to_string();
                let index_key = node
                    .exec
                    .get("loop_index_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("loop.index")
                    .to_string();
                return control::LoopConfig::ForEach {
                    collection: collection.to_string(),
                    item_key,
                    index_key,
                };
            }
        }
    }
    // Default: single execution (same as sequence)
    control::LoopConfig::Repeat(1)
}

/// Group nodes into parallel waves based on dependency depth.
///
/// Handles cycles by identifying back-edges (edges that create cycles) and
/// excluding them from the dependency ordering. Back-edges are typically
/// feedback loops (retry, human-in-the-loop) where a downstream node routes
/// back to an earlier node. These are valid workflow patterns — the executor
/// handles them via subgraph loop directives.
pub(crate) fn compute_waves(graph: &Graph) -> Result<Vec<Vec<NodeId>>, ExecutionError> {
    // Try direct toposort first (fast path for DAGs)
    let topo = match toposort(&graph.inner, None) {
        Ok(t) => t,
        Err(_) => {
            // Graph has cycles — find and remove DFS back-edges to make it a DAG.
            // Back-edges are the edges that create cycles; removing them breaks all
            // cycles while preserving the forward dependency structure.
            let mut filtered = graph.inner.clone();
            let back_edges = find_back_edges(&filtered);

            let removed_count = back_edges.len();
            // Remove in reverse index order to keep edge indices stable
            let mut sorted_edges = back_edges;
            sorted_edges.sort_unstable();
            for e in sorted_edges.into_iter().rev() {
                filtered.remove_edge(e);
            }
            debug!(removed_count, "removed back-edges to break cycles");

            toposort(&filtered, None).map_err(|cycle| {
                ExecutionError::ValidationFailed(format!(
                    "unresolvable cycle at node '{}' (persists after removing back-edges)",
                    graph.inner[cycle.node_id()].id
                ))
            })?
        }
    };

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

/// Find DFS back-edges in a directed graph. Back-edges are edges that point
/// from a node to one of its ancestors in the DFS tree — these are exactly
/// the edges whose removal breaks all cycles.
fn find_back_edges(
    graph: &petgraph::stable_graph::StableDiGraph<
        crate::graph::node::Node,
        crate::graph::edge::EdgeData,
    >,
) -> Vec<petgraph::graph::EdgeIndex> {
    use petgraph::stable_graph::EdgeIndex;

    let mut back_edges = Vec::new();
    let mut visiting = HashSet::new(); // Currently on the DFS stack (grey nodes)
    let mut visited = HashSet::new(); // Fully processed (black nodes)

    fn dfs(
        node: NodeIndex,
        graph: &petgraph::stable_graph::StableDiGraph<
            crate::graph::node::Node,
            crate::graph::edge::EdgeData,
        >,
        visiting: &mut HashSet<NodeIndex>,
        visited: &mut HashSet<NodeIndex>,
        back_edges: &mut Vec<EdgeIndex>,
    ) {
        visiting.insert(node);
        let neighbors: Vec<_> = graph
            .edges_directed(node, Direction::Outgoing)
            .map(|e| (e.id(), e.target()))
            .collect();
        for (edge_id, target) in neighbors {
            if visiting.contains(&target) {
                back_edges.push(edge_id);
            } else if !visited.contains(&target) {
                dfs(target, graph, visiting, visited, back_edges);
            }
        }
        visiting.remove(&node);
        visited.insert(node);
    }

    for node in graph.node_indices() {
        if !visited.contains(&node) {
            dfs(node, graph, &mut visiting, &mut visited, &mut back_edges);
        }
    }

    back_edges
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

/// Mark downstream (transitive successors) of a failed/cancelled node as Cancelled.
///
/// Convergence-aware: skips nodes that have other predecessors still
/// potentially live (not Failed/Cancelled). This prevents a cancelled
/// conditional branch from eagerly cancelling the merge node while the
/// other branch is still running.
pub(crate) fn cancel_downstream(graph: &Graph, failed_id: &NodeId, ctx: &ExecutionContext) {
    let mut stack = vec![failed_id.clone()];
    let mut visited = HashSet::new();
    visited.insert(failed_id.clone());

    while let Some(current) = stack.pop() {
        for successor in graph.successors(&current) {
            if visited.insert(successor.id.clone()) {
                if ctx.get_state(&successor.id.0).is_terminal() {
                    continue;
                }
                // Don't cancel convergence nodes that have live predecessors
                let preds = graph.predecessors(&successor.id);
                let all_dead = preds.iter().all(|p| {
                    matches!(
                        ctx.get_state(&p.id.0),
                        NodeState::Failed | NodeState::Cancelled
                    )
                });
                if all_dead {
                    let _ = ctx.set_state(&successor.id.0, NodeState::Cancelled);
                    stack.push(successor.id.clone());
                }
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
        assert_eq!(
            result.node_outputs["C"]["trace"],
            Value::String("ABC".into())
        );
    }

    #[tokio::test]
    async fn execute_diamond_dependency() {
        let mut graph = Graph::new();
        for id in ["A", "B", "C", "D"] {
            graph
                .add_node(Node::new(id, id).with_handler("pass"))
                .unwrap();
        }
        graph
            .add_edge(&"A".into(), "", &"B".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"A".into(), "", &"C".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"B".into(), "", &"D".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"C".into(), "", &"D".into(), "", None)
            .unwrap();

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
        graph
            .add_edge(&"A".into(), "", &"B".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"A".into(), "", &"C".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"A".into(), "", &"D".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"B".into(), "", &"E".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"C".into(), "", &"E".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"D".into(), "", &"E".into(), "", None)
            .unwrap();

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
        graph
            .add_edge(&"A".into(), "", &"B".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"A".into(), "", &"C".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"B".into(), "", &"D".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"C".into(), "", &"D".into(), "", None)
            .unwrap();

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
        graph
            .add_node(Node::new("A", "A").with_handler("ok"))
            .unwrap();
        graph
            .add_node(Node::new("B", "B").with_handler("fail"))
            .unwrap();
        graph
            .add_node(Node::new("C", "C").with_handler("ok"))
            .unwrap();
        graph
            .add_edge(&"A".into(), "", &"B".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"B".into(), "", &"C".into(), "", None)
            .unwrap();

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
        graph
            .add_node(Node::new("A", "A").with_handler("ok"))
            .unwrap();
        graph
            .add_node(Node::new("B", "B").with_handler("fail"))
            .unwrap();
        graph
            .add_node(Node::new("C", "C").with_handler("fail"))
            .unwrap();
        graph
            .add_node(Node::new("D", "D").with_handler("ok"))
            .unwrap();
        graph
            .add_edge(&"A".into(), "", &"B".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"A".into(), "", &"C".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"B".into(), "", &"D".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"C".into(), "", &"D".into(), "", None)
            .unwrap();

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
        graph
            .add_edge(&"A".into(), "", &"B".into(), "", None)
            .unwrap();

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
        let result = executor.execute(&g, &HandlerRegistry::new()).await.unwrap();
        assert_eq!(result.node_states["A"], NodeState::Completed);

        // Linear chain with error propagation
        let mut g = Graph::new();
        g.add_node(Node::new("X", "X").with_handler("ok")).unwrap();
        g.add_node(Node::new("Y", "Y").with_handler("fail"))
            .unwrap();
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

        graph
            .add_node(Node::new("YES", "Yes").with_handler("ok"))
            .unwrap();
        graph
            .add_node(Node::new("NO", "No").with_handler("ok"))
            .unwrap();

        graph
            .add_edge(&"PROD".into(), "flag", &"BR".into(), "flag", None)
            .unwrap();
        graph
            .add_edge(&"BR".into(), "", &"YES".into(), "", Some("yes".into()))
            .unwrap();
        graph
            .add_edge(&"BR".into(), "", &"NO".into(), "", Some("no".into()))
            .unwrap();

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
        handlers.insert("pass_through".into(), sync_handler(|_, inputs| Ok(inputs)));

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

        graph
            .add_node(Node::new("PROD", "Producer").with_handler("produce_false"))
            .unwrap();

        let mut branch = Node::new("BR", "Branch").with_handler("pass_through");
        branch.config = serde_json::json!({ "guard": "inputs.flag == true" });
        graph.add_node(branch).unwrap();

        graph
            .add_node(Node::new("YES", "Yes").with_handler("ok"))
            .unwrap();
        graph
            .add_node(Node::new("NO", "No").with_handler("ok"))
            .unwrap();

        graph
            .add_edge(&"PROD".into(), "flag", &"BR".into(), "flag", None)
            .unwrap();
        graph
            .add_edge(&"BR".into(), "", &"YES".into(), "", Some("yes".into()))
            .unwrap();
        graph
            .add_edge(&"BR".into(), "", &"NO".into(), "", Some("no".into()))
            .unwrap();

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
        handlers.insert("pass_through".into(), sync_handler(|_, inputs| Ok(inputs)));

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
        graph
            .add_node(Node::new("A", "A").with_handler("trace"))
            .unwrap();
        graph
            .add_node(Node::new("B", "B").with_handler("trace"))
            .unwrap();

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
        graph
            .add_node(Node::new("FAST", "Fast").with_handler("fast"))
            .unwrap();
        graph
            .add_node(Node::new("SLOW", "Slow").with_handler("slow"))
            .unwrap();

        graph.add_subgraph(crate::graph::Subgraph {
            id: "race1".into(),
            label: "race: candidates".into(),
            directive: SubgraphDirective::Race,
            nodes: vec!["FAST".into(), "SLOW".into()],
            children: Vec::new(),
        });

        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "fast".into(),
            sync_handler(|_, _| {
                let mut out = Outputs::new();
                out.insert("result".into(), Value::String("fast_won".into()));
                Ok(out)
            }),
        );
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
        graph
            .add_node(Node::new("S1", "Step 1").with_handler("trace"))
            .unwrap();
        graph
            .add_node(Node::new("S2", "Step 2").with_handler("trace"))
            .unwrap();
        graph
            .add_node(Node::new("S3", "Step 3").with_handler("trace"))
            .unwrap();
        graph
            .add_edge(&"S1".into(), "", &"S2".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"S2".into(), "", &"S3".into(), "", None)
            .unwrap();

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
        graph
            .add_node(Node::new("S1", "S1").with_handler("ok"))
            .unwrap();
        graph
            .add_node(Node::new("S2", "S2").with_handler("fail"))
            .unwrap();
        graph
            .add_node(Node::new("S3", "S3").with_handler("ok"))
            .unwrap();
        graph
            .add_edge(&"S1".into(), "", &"S2".into(), "", None)
            .unwrap();
        graph
            .add_edge(&"S2".into(), "", &"S3".into(), "", None)
            .unwrap();

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
            assert_eq!(result.node_states[&format!("N{i}")], NodeState::Completed);
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

    #[test]
    fn parse_loop_config_foreach() {
        let mut graph = Graph::new();
        let mut node = Node::new("L", "Loop body");
        node.exec = serde_json::json!({
            "loop_foreach": "wf:inputs.files",
            "loop_item_key": "loop.file",
            "loop_index_key": "loop.idx"
        });
        graph.add_node(node).unwrap();

        let config = parse_loop_config(&graph, &[NodeId::new("L")]);
        match config {
            control::LoopConfig::ForEach {
                collection,
                item_key,
                index_key,
            } => {
                assert_eq!(collection, "wf:inputs.files");
                assert_eq!(item_key, "loop.file");
                assert_eq!(index_key, "loop.idx");
            }
            other => panic!("expected ForEach, got {other:?}"),
        }
    }

    #[test]
    fn parse_loop_config_foreach_defaults() {
        let mut graph = Graph::new();
        let mut node = Node::new("L", "Loop body");
        node.exec = serde_json::json!({
            "loop_foreach": "items"
        });
        graph.add_node(node).unwrap();

        let config = parse_loop_config(&graph, &[NodeId::new("L")]);
        match config {
            control::LoopConfig::ForEach {
                collection,
                item_key,
                index_key,
            } => {
                assert_eq!(collection, "items");
                assert_eq!(item_key, "loop.item");
                assert_eq!(index_key, "loop.index");
            }
            other => panic!("expected ForEach, got {other:?}"),
        }
    }

    #[test]
    fn parse_loop_config_loop_over_alias() {
        let mut graph = Graph::new();
        let mut node = Node::new("L", "Loop body");
        node.exec = serde_json::json!({
            "loop_over": "form.sections"
        });
        graph.add_node(node).unwrap();

        let config = parse_loop_config(&graph, &[NodeId::new("L")]);
        match config {
            control::LoopConfig::ForEach { collection, .. } => {
                assert_eq!(collection, "form.sections");
            }
            other => panic!("expected ForEach via loop_over alias, got {other:?}"),
        }
    }

    // -- 2.T.2: Property-based tests for control flow --

    mod proptest_control_flow {
        use super::*;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        fn build_random_dag(n: usize, edges: &[(usize, usize)]) -> Graph {
            let mut graph = Graph::new();
            for i in 0..n {
                graph
                    .add_node(Node::new(format!("N{i}"), format!("Node {i}")).with_handler("h"))
                    .unwrap();
            }
            for (src, tgt) in edges {
                if *src < *tgt && *src < n && *tgt < n {
                    let src_id: NodeId = format!("N{src}").into();
                    let tgt_id: NodeId = format!("N{tgt}").into();
                    let _ = graph.add_edge(&src_id, "out", &tgt_id, "in", None);
                }
            }
            graph
        }

        #[test]
        fn prop_all_nodes_reach_terminal_state() {
            use proptest::prelude::*;
            use proptest::test_runner::TestRunner;

            let mut runner = TestRunner::new(proptest::test_runner::Config {
                cases: 50,
                ..Default::default()
            });

            runner
                .run(
                    &(
                        2..10usize,
                        proptest::collection::vec((0..10usize, 0..10usize), 0..15),
                    ),
                    |(n, edges)| {
                        let rt = tokio::runtime::Runtime::new().unwrap();
                        rt.block_on(async {
                            let graph = build_random_dag(n, &edges);
                            let mut handlers = HandlerRegistry::new();
                            handlers.insert(
                                "h".into(),
                                crate::execute::sync_handler(|_, _| Ok(Outputs::new())),
                            );

                            let result = TopologicalExecutor::new()
                                .execute(&graph, &handlers)
                                .await
                                .unwrap();

                            for i in 0..n {
                                let id = format!("N{i}");
                                let state = result.node_states.get(&id).unwrap_or(&NodeState::Idle);
                                prop_assert!(
                                    state.is_terminal(),
                                    "node {} in non-terminal state {:?}",
                                    id,
                                    state
                                );
                            }
                            Ok(())
                        })
                    },
                )
                .unwrap();
        }

        #[test]
        fn prop_no_double_execution() {
            use proptest::prelude::*;
            use proptest::test_runner::TestRunner;

            let mut runner = TestRunner::new(proptest::test_runner::Config {
                cases: 50,
                ..Default::default()
            });

            runner
                .run(
                    &(
                        2..8usize,
                        proptest::collection::vec((0..8usize, 0..8usize), 0..12),
                    ),
                    |(n, edges)| {
                        let rt = tokio::runtime::Runtime::new().unwrap();
                        rt.block_on(async {
                            let call_count = Arc::new(AtomicUsize::new(0));
                            let counter = call_count.clone();

                            let graph = build_random_dag(n, &edges);
                            let mut handlers = HandlerRegistry::new();
                            handlers.insert(
                                "h".into(),
                                crate::execute::sync_handler(move |_, _| {
                                    counter.fetch_add(1, AtomicOrdering::SeqCst);
                                    Ok(Outputs::new())
                                }),
                            );

                            let _ = TopologicalExecutor::new()
                                .execute(&graph, &handlers)
                                .await
                                .unwrap();

                            let total = call_count.load(AtomicOrdering::SeqCst);
                            prop_assert_eq!(
                                total,
                                n,
                                "double execution: {} calls for {} nodes",
                                total,
                                n
                            );
                            Ok(())
                        })
                    },
                )
                .unwrap();
        }

        #[test]
        fn prop_cancellation_propagates() {
            use proptest::prelude::*;
            use proptest::test_runner::TestRunner;

            let mut runner = TestRunner::new(proptest::test_runner::Config {
                cases: 50,
                ..Default::default()
            });

            runner
                .run(
                    &(
                        3..8usize,
                        proptest::collection::vec((0..8usize, 0..8usize), 0..10),
                    ),
                    |(n, edges)| {
                        let rt = tokio::runtime::Runtime::new().unwrap();
                        rt.block_on(async {
                            let cancel = CancellationToken::new();
                            cancel.cancel();

                            let graph = build_random_dag(n, &edges);
                            let mut handlers = HandlerRegistry::new();
                            handlers.insert(
                                "h".into(),
                                crate::execute::sync_handler(|_, _| Ok(Outputs::new())),
                            );

                            let result = TopologicalExecutor::with_cancel(cancel)
                                .execute(&graph, &handlers)
                                .await
                                .unwrap();

                            for i in 0..n {
                                let id = format!("N{i}");
                                let state = result
                                    .node_states
                                    .get(&id)
                                    .copied()
                                    .unwrap_or(NodeState::Idle);
                                prop_assert!(
                                    state == NodeState::Cancelled || state == NodeState::Idle,
                                    "node {} should be cancelled/idle, got {:?}",
                                    id,
                                    state
                                );
                            }
                            Ok(())
                        })
                    },
                )
                .unwrap();
        }

        #[test]
        fn prop_dependency_order_respected() {
            use proptest::prelude::*;
            use proptest::test_runner::TestRunner;

            let mut runner = TestRunner::new(proptest::test_runner::Config {
                cases: 30,
                ..Default::default()
            });

            runner
                .run(&(2..6usize), |chain_len| {
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    rt.block_on(async {
                        let execution_order = Arc::new(std::sync::Mutex::new(Vec::new()));

                        let mut graph = Graph::new();
                        for i in 0..chain_len {
                            graph
                                .add_node(
                                    Node::new(format!("N{i}"), format!("N{i}"))
                                        .with_handler("record"),
                                )
                                .unwrap();
                        }
                        for i in 0..chain_len - 1 {
                            graph
                                .add_edge(
                                    &format!("N{i}").into(),
                                    "out",
                                    &format!("N{}", i + 1).into(),
                                    "in",
                                    None,
                                )
                                .unwrap();
                        }

                        let order_clone = execution_order.clone();
                        let mut handlers = HandlerRegistry::new();
                        handlers.insert(
                            "record".into(),
                            crate::execute::sync_handler(move |node, _| {
                                order_clone.lock().unwrap().push(node.id.0.clone());
                                Ok(Outputs::new())
                            }),
                        );

                        let _ = TopologicalExecutor::new()
                            .execute(&graph, &handlers)
                            .await
                            .unwrap();

                        let order = execution_order.lock().unwrap();
                        prop_assert_eq!(order.len(), chain_len, "not all chain nodes executed");
                        for i in 1..chain_len {
                            let prev = order.iter().position(|id| *id == format!("N{}", i - 1));
                            let curr = order.iter().position(|id| *id == format!("N{i}"));
                            if let (Some(p), Some(c)) = (prev, curr) {
                                prop_assert!(p < c, "N{} before N{}", i - 1, i);
                            }
                        }
                        Ok(())
                    })
                })
                .unwrap();
        }
    }
}
