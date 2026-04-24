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
    auto_install_auth_registry, ExecutionError, ExecutionResult, Executor, HandlerRegistry,
    NodeHandler,
};
use crate::graph::node::NodeId;
use crate::graph::Graph;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Reactive/dataflow executor: fires nodes when all inputs are satisfied,
/// propagates changes downstream. No pre-computed wave ordering.
///
/// Nodes with no incoming edges fire immediately. When a node completes,
/// its downstream neighbors are checked — if all their predecessors have
/// completed, they become ready and fire next. This naturally handles
/// diamond dependencies and fan-out/fan-in patterns.
pub struct ReactiveExecutor {
    cancel_token: CancellationToken,
    concurrency: ConcurrencyLimits,
}

impl ReactiveExecutor {
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
}

impl Default for ReactiveExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for ReactiveExecutor {
    fn execute<'a>(
        &'a self,
        graph: &'a Graph,
        handlers: &'a HandlerRegistry,
    ) -> Pin<Box<dyn Future<Output = Result<ExecutionResult, ExecutionError>> + Send + 'a>> {
        Box::pin(execute_reactive(
            graph,
            handlers,
            self.cancel_token.clone(),
            self.concurrency.clone(),
        ))
    }
}

/// Check if all predecessors of a node are in a terminal state.
fn all_inputs_satisfied(graph: &Graph, node_id: &NodeId, ctx: &ExecutionContext) -> bool {
    graph
        .predecessors(node_id)
        .iter()
        .all(|pred| ctx.get_state(&pred.id.0).is_terminal())
}

/// Check if a predecessor failed or was cancelled, meaning this node should be cancelled too.
fn has_failed_predecessor(graph: &Graph, node_id: &NodeId, ctx: &ExecutionContext) -> bool {
    graph.predecessors(node_id).iter().any(|pred| {
        matches!(
            ctx.get_state(&pred.id.0),
            NodeState::Failed | NodeState::Cancelled
        )
    })
}

async fn execute_reactive(
    graph: &Graph,
    handlers: &HandlerRegistry,
    cancel_token: CancellationToken,
    concurrency: ConcurrencyLimits,
) -> Result<ExecutionResult, ExecutionError> {
    let start = Instant::now();
    let ctx = Arc::new(ExecutionContext::with_concurrency(
        cancel_token,
        concurrency,
    ));

    auto_install_auth_registry(graph, &ctx)?;
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

    let passthrough: Arc<dyn NodeHandler> = Arc::new(PassthroughHandler);

    // Seed the ready queue with source nodes (no predecessors)
    let mut ready: VecDeque<NodeId> = VecDeque::new();
    for node in graph.nodes() {
        if graph.predecessors(&node.id).is_empty() {
            ready.push_back(node.id.clone());
        }
    }

    // Track which nodes have been queued to avoid duplicates
    let mut queued: HashSet<NodeId> = ready.iter().cloned().collect();

    // Process ready nodes: fire them, then check downstream
    while !ready.is_empty() {
        if ctx.is_cancelled() {
            // Cancel all non-terminal nodes
            for node in graph.nodes() {
                if !ctx.get_state(&node.id.0).is_terminal() {
                    let _ = ctx.set_state(&node.id.0, NodeState::Cancelled);
                }
            }
            break;
        }

        // Collect the current batch of ready nodes for concurrent execution
        let batch: Vec<NodeId> = ready.drain(..).collect();
        let mut handles = Vec::new();

        for node_id in &batch {
            if ctx.get_state(&node_id.0).is_terminal() {
                continue;
            }

            // If a predecessor failed/cancelled, cancel this node and propagate
            if has_failed_predecessor(graph, node_id, &ctx) {
                let _ = ctx.set_state(&node_id.0, NodeState::Cancelled);
                cancel_downstream(graph, node_id, &ctx);
                // Still check downstream for nodes that might now have all inputs terminal
                enqueue_ready_successors(graph, node_id, &ctx, &mut ready, &mut queued);
                continue;
            }

            // Check branch blocking
            if is_branch_blocked(graph, node_id, &ctx) {
                let _ = ctx.set_state(&node_id.0, NodeState::Cancelled);
                cancel_downstream(graph, node_id, &ctx);
                enqueue_ready_successors(graph, node_id, &ctx, &mut ready, &mut queued);
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

            let _global_permit = ctx.concurrency().acquire().await;

            ctx.set_state(&node_id_str, NodeState::Pending)
                .map_err(|e| ExecutionError::ValidationFailed(e.to_string()))?;

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

        // Await all tasks in this batch
        for handle in handles {
            let (node_id, outcome) = handle
                .await
                .map_err(|e| ExecutionError::ValidationFailed(format!("task panic: {e}")))?;

            let nid = NodeId::new(&node_id);

            match outcome {
                Ok(outputs) => {
                    handle_branch_decision(graph, &node_id, &outputs, &ctx, None).await;
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
                    cancel_downstream(graph, &nid, &ctx);
                }
            }

            // Check downstream: enqueue successors whose inputs are all satisfied
            enqueue_ready_successors(graph, &nid, &ctx, &mut ready, &mut queued);
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

/// Check all successors of a node and enqueue those whose inputs are all in terminal state.
fn enqueue_ready_successors(
    graph: &Graph,
    node_id: &NodeId,
    ctx: &ExecutionContext,
    ready: &mut VecDeque<NodeId>,
    queued: &mut HashSet<NodeId>,
) {
    for successor in graph.successors(node_id) {
        if queued.contains(&successor.id) {
            continue;
        }
        if all_inputs_satisfied(graph, &successor.id, ctx) {
            queued.insert(successor.id.clone());
            ready.push_back(successor.id.clone());
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
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

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

    // -- Core reactive execution tests --

    #[tokio::test]
    async fn reactive_linear_chain() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("trace"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("trace"))
            .unwrap();
        g.add_node(Node::new("C", "C").with_handler("trace"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();

        let result = ReactiveExecutor::new()
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
    async fn reactive_diamond_dependency() {
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

        let result = ReactiveExecutor::new()
            .execute(&g, &handlers)
            .await
            .unwrap();

        for state in result.node_states.values() {
            assert_eq!(*state, NodeState::Completed);
        }
    }

    #[tokio::test]
    async fn reactive_fan_out_fan_in() {
        let mut g = Graph::new();
        for id in ["A", "B", "C", "D", "E"] {
            g.add_node(Node::new(id, id).with_handler("trace")).unwrap();
        }
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"C".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"D".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"E".into(), "", None).unwrap();
        g.add_edge(&"C".into(), "", &"E".into(), "", None).unwrap();
        g.add_edge(&"D".into(), "", &"E".into(), "", None).unwrap();

        let result = ReactiveExecutor::new()
            .execute(&g, &trace_handlers())
            .await
            .unwrap();

        assert_eq!(result.node_states.len(), 5);
        for state in result.node_states.values() {
            assert_eq!(*state, NodeState::Completed);
        }
    }

    #[tokio::test]
    async fn reactive_empty_graph() {
        let result = ReactiveExecutor::new()
            .execute(&Graph::new(), &HandlerRegistry::new())
            .await
            .unwrap();
        assert!(result.node_states.is_empty());
    }

    #[tokio::test]
    async fn reactive_no_re_execution_of_unchanged_branches() {
        // A → B, A → C. B and C are independent.
        // After A fires, B and C should each fire exactly once.
        let counter_b = Arc::new(AtomicUsize::new(0));
        let counter_c = Arc::new(AtomicUsize::new(0));

        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("count_b"))
            .unwrap();
        g.add_node(Node::new("C", "C").with_handler("count_c"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"C".into(), "", None).unwrap();

        let cb = counter_b.clone();
        let cc = counter_c.clone();
        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "count_b".into(),
            sync_handler(move |_, inputs| {
                cb.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(inputs)
            }),
        );
        handlers.insert(
            "count_c".into(),
            sync_handler(move |_, inputs| {
                cc.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(inputs)
            }),
        );

        let result = ReactiveExecutor::new()
            .execute(&g, &handlers)
            .await
            .unwrap();

        assert_eq!(counter_b.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(counter_c.load(AtomicOrdering::SeqCst), 1);
        for state in result.node_states.values() {
            assert_eq!(*state, NodeState::Completed);
        }
    }

    // -- Error propagation --

    #[tokio::test]
    async fn reactive_error_cascades_downstream() {
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

        let result = ReactiveExecutor::new()
            .execute(&g, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Failed);
        assert_eq!(result.node_states["C"], NodeState::Cancelled);
    }

    // -- Cancellation --

    #[tokio::test]
    async fn reactive_global_cancellation() {
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

        let result = ReactiveExecutor::with_cancel(token)
            .execute(&g, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["A"], NodeState::Completed);
        assert_eq!(result.node_states["B"], NodeState::Cancelled);
    }

    // -- Branch/conditional --

    #[tokio::test]
    async fn reactive_branch_follows_yes_path() {
        let mut g = Graph::new();
        g.add_node(Node::new("PROD", "Prod").with_handler("produce_true"))
            .unwrap();
        let mut branch = Node::new("BR", "Branch").with_handler("pass");
        branch.config = serde_json::json!({ "guard": "inputs.flag == true" });
        g.add_node(branch).unwrap();
        g.add_node(Node::new("YES", "Yes").with_handler("ok"))
            .unwrap();
        g.add_node(Node::new("NO", "No").with_handler("ok"))
            .unwrap();
        g.add_edge(&"PROD".into(), "flag", &"BR".into(), "flag", None)
            .unwrap();
        g.add_edge(&"BR".into(), "", &"YES".into(), "", Some("yes".into()))
            .unwrap();
        g.add_edge(&"BR".into(), "", &"NO".into(), "", Some("no".into()))
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("ok".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "produce_true".into(),
            sync_handler(|_, _| {
                let mut out = Outputs::new();
                out.insert("flag".into(), Value::Bool(true));
                Ok(out)
            }),
        );

        let result = ReactiveExecutor::new()
            .execute(&g, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["YES"], NodeState::Completed);
        assert_eq!(result.node_states["NO"], NodeState::Cancelled);
    }

    // -- Data flow --

    #[tokio::test]
    async fn reactive_data_flows_through_ports() {
        let mut g = Graph::new();
        g.add_node(Node::new("SRC", "Source").with_handler("produce"))
            .unwrap();
        g.add_node(Node::new("DST", "Dest").with_handler("consume"))
            .unwrap();
        g.add_edge(&"SRC".into(), "value", &"DST".into(), "input", None)
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

        let result = ReactiveExecutor::new()
            .execute(&g, &handlers)
            .await
            .unwrap();

        assert_eq!(result.node_states["DST"], NodeState::Completed);
    }

    // -- Executor trait conformance --

    #[tokio::test]
    async fn reactive_executor_trait_conformance() {
        let executor: Box<dyn Executor> = Box::new(ReactiveExecutor::new());

        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        let result = executor.execute(&g, &HandlerRegistry::new()).await.unwrap();
        assert_eq!(result.node_states["A"], NodeState::Completed);

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

    // -- Concurrency --

    #[tokio::test]
    async fn reactive_concurrency_limit_respected() {
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        // 4 independent nodes (all sources) with max_parallelism: 2
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

        let result = ReactiveExecutor::new()
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
