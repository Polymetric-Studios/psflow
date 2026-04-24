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
    NodeHandler, Outputs,
};
use crate::graph::node::NodeId;
use crate::graph::Graph;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// An external event to push into a graph entry node.
#[derive(Debug, Clone)]
pub struct EventMessage {
    /// The node ID to push this event into.
    pub target_node: String,
    /// The data to provide as the node's inputs.
    pub data: Outputs,
}

/// Sender handle for pushing events into an event-driven executor.
///
/// Obtained from `EventDrivenExecutor::sender()` before calling `execute()`.
/// Can be cloned and shared across threads.
#[derive(Clone)]
pub struct EventSender {
    tx: mpsc::Sender<EventMessage>,
}

impl EventSender {
    /// Send an event to a target node. Returns an error if the executor has shut down.
    pub async fn send(
        &self,
        msg: EventMessage,
    ) -> Result<(), mpsc::error::SendError<EventMessage>> {
        self.tx.send(msg).await
    }

    /// Try to send an event without waiting. Returns an error if the channel is full or closed.
    pub fn try_send(
        &self,
        msg: EventMessage,
    ) -> Result<(), mpsc::error::TrySendError<EventMessage>> {
        self.tx.try_send(msg)
    }
}

/// Event-driven executor: external events push data into designated entry nodes,
/// triggering downstream execution via channels.
///
/// Entry nodes receive data from `EventSender::send()`. When an event arrives,
/// the target node fires with the event data as inputs, then downstream nodes
/// propagate reactively (same as `ReactiveExecutor`).
///
/// The executor runs until cancelled via its `CancellationToken` or until all
/// `EventSender` handles are dropped (channel closed).
///
/// Construct with `EventDrivenExecutor::new()` which returns both the executor
/// and a sender. Clone the sender to create additional event producers.
pub struct EventDrivenExecutor {
    cancel_token: CancellationToken,
    concurrency: ConcurrencyLimits,
    event_rx: tokio::sync::Mutex<mpsc::Receiver<EventMessage>>,
}

const DEFAULT_CHANNEL_CAPACITY: usize = 256;

impl EventDrivenExecutor {
    /// Create a new event-driven executor and its corresponding event sender.
    ///
    /// The sender is the only way to push events into the executor. Clone it
    /// to create multiple producers. When all senders are dropped, the executor
    /// shuts down gracefully.
    pub fn new() -> (Self, EventSender) {
        Self::with_capacity(DEFAULT_CHANNEL_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> (Self, EventSender) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            Self {
                cancel_token: CancellationToken::new(),
                concurrency: ConcurrencyLimits::new(),
                event_rx: tokio::sync::Mutex::new(rx),
            },
            EventSender { tx },
        )
    }

    pub fn with_cancel(token: CancellationToken) -> (Self, EventSender) {
        let (tx, rx) = mpsc::channel(DEFAULT_CHANNEL_CAPACITY);
        (
            Self {
                cancel_token: token,
                concurrency: ConcurrencyLimits::new(),
                event_rx: tokio::sync::Mutex::new(rx),
            },
            EventSender { tx },
        )
    }

    pub fn with_concurrency(mut self, limits: ConcurrencyLimits) -> Self {
        self.concurrency = limits;
        self
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }
}

impl Executor for EventDrivenExecutor {
    fn execute<'a>(
        &'a self,
        graph: &'a Graph,
        handlers: &'a HandlerRegistry,
    ) -> Pin<Box<dyn Future<Output = Result<ExecutionResult, ExecutionError>> + Send + 'a>> {
        Box::pin(execute_event_driven(
            graph,
            handlers,
            &self.event_rx,
            self.cancel_token.clone(),
            self.concurrency.clone(),
        ))
    }
}

async fn execute_event_driven(
    graph: &Graph,
    handlers: &HandlerRegistry,
    event_rx: &tokio::sync::Mutex<mpsc::Receiver<EventMessage>>,
    cancel_token: CancellationToken,
    concurrency: ConcurrencyLimits,
) -> Result<ExecutionResult, ExecutionError> {
    let start = Instant::now();
    let ctx = Arc::new(ExecutionContext::with_concurrency(
        cancel_token.clone(),
        concurrency,
    ));

    auto_install_auth_registry(graph, &ctx)?;
    crate::execute::validation::validate_graph(graph, handlers, &ctx)?;
    ctx.emit(ExecutionEvent::ExecutionStarted { timestamp: start });

    let passthrough: Arc<dyn NodeHandler> = Arc::new(PassthroughHandler);
    let mut rx = event_rx.lock().await;

    // Event loop: wait for events, fire target node, propagate downstream
    loop {
        let event = tokio::select! {
            _ = cancel_token.cancelled() => {
                // Cancel all non-terminal nodes
                for node in graph.nodes() {
                    if !ctx.get_state(&node.id.0).is_terminal() {
                        let _ = ctx.set_state(&node.id.0, NodeState::Cancelled);
                    }
                }
                break;
            }
            msg = rx.recv() => {
                match msg {
                    Some(e) => e,
                    None => break, // Channel closed, all senders dropped
                }
            }
        };

        // Validate the target node exists
        let target_id = NodeId::new(&event.target_node);
        if graph.node(&target_id).is_none() {
            ctx.emit(ExecutionEvent::NodeFailed {
                node_id: event.target_node.clone(),
                error: NodeError::Failed {
                    source_message: None,
                    message: format!(
                        "event target node '{}' not found in graph",
                        event.target_node
                    ),
                    recoverable: false,
                },
            });
            continue;
        }

        // Reset the target node AND all transitive successors so the entire
        // downstream subgraph can re-execute on each event. Without this,
        // repeated events would fail because downstream nodes are still in
        // terminal state (Completed/Failed) and cannot transition to Pending.
        let nodes_to_reset = collect_transitive_successors(graph, &target_id);
        ctx.reset_states(nodes_to_reset.iter().map(|id| id.0.as_str()));

        // Store the event data as the entry node's outputs. propagate_from()
        // reads these back as inputs for the entry node's handler. The handler
        // then executes and its actual outputs overwrite this stored data.
        ctx.store_outputs(&event.target_node, event.data);

        // Fire the target node and propagate downstream reactively
        propagate_from(graph, handlers, &ctx, &passthrough, &target_id).await?;
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

/// Fire a node and propagate downstream reactively.
async fn propagate_from(
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &Arc<ExecutionContext>,
    passthrough: &Arc<dyn NodeHandler>,
    start_node: &NodeId,
) -> Result<(), ExecutionError> {
    let mut ready: VecDeque<NodeId> = VecDeque::new();
    ready.push_back(start_node.clone());
    let mut queued: HashSet<NodeId> = HashSet::new();
    queued.insert(start_node.clone());

    while let Some(node_id) = ready.pop_front() {
        if ctx.is_cancelled() {
            break;
        }

        // Skip nodes already in terminal state from this propagation round
        // (e.g., cancelled by a failed sibling's cancel_downstream).
        // All nodes were reset to Idle before propagation started.
        if ctx.get_state(&node_id.0).is_terminal() {
            for succ in graph.successors(&node_id) {
                if !queued.contains(&succ.id) && all_inputs_terminal(graph, &succ.id, ctx) {
                    queued.insert(succ.id.clone());
                    ready.push_back(succ.id.clone());
                }
            }
            continue;
        }

        let node = match graph.node(&node_id) {
            Some(n) => n,
            None => continue,
        };

        // Check branch blocking
        if is_branch_blocked(graph, &node_id, ctx) {
            let _ = ctx.set_state(&node_id.0, NodeState::Cancelled);
            cancel_downstream(graph, &node_id, ctx);
            continue;
        }

        let handler: Arc<dyn NodeHandler> = node
            .handler
            .as_ref()
            .and_then(|name| handlers.get(name))
            .cloned()
            .unwrap_or_else(|| passthrough.clone());

        // For the entry node, use stored outputs as inputs.
        // For downstream nodes, collect from predecessors normally.
        let inputs = if &node_id == start_node {
            ctx.get_outputs(&node_id.0).unwrap_or_default()
        } else {
            collect_inputs(graph, &node_id, ctx)
        };

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

        let handle = tokio::spawn(async move {
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
        });

        let (node_id_str, outcome) = handle
            .await
            .map_err(|e| ExecutionError::ValidationFailed(format!("task panic: {e}")))?;

        let nid = NodeId::new(&node_id_str);

        match outcome {
            Ok(outputs) => {
                handle_branch_decision(graph, &node_id_str, &outputs, ctx, None).await;
                ctx.store_outputs(&node_id_str, outputs.clone());
                ctx.emit(ExecutionEvent::NodeCompleted {
                    node_id: node_id_str.clone(),
                    outputs,
                });
                let _ = ctx.set_state(&node_id_str, NodeState::Completed);

                // Enqueue ready successors
                for succ in graph.successors(&nid) {
                    if !queued.contains(&succ.id) && all_inputs_terminal(graph, &succ.id, ctx) {
                        queued.insert(succ.id.clone());
                        ready.push_back(succ.id.clone());
                    }
                }
            }
            Err(NodeError::Cancelled { .. }) => {
                let _ = ctx.set_state(&node_id_str, NodeState::Cancelled);
            }
            Err(ref error) => {
                ctx.emit(ExecutionEvent::NodeFailed {
                    node_id: node_id_str.clone(),
                    error: error.clone(),
                });
                let _ = ctx.set_state(&node_id_str, NodeState::Failed);
                cancel_downstream(graph, &nid, ctx);
            }
        }
    }

    Ok(())
}

/// Collect a node and all its transitive successors (for resetting before re-execution).
fn collect_transitive_successors(graph: &Graph, start: &NodeId) -> Vec<NodeId> {
    let mut result = vec![start.clone()];
    let mut visited: HashSet<NodeId> = HashSet::new();
    visited.insert(start.clone());
    let mut stack = vec![start.clone()];

    while let Some(current) = stack.pop() {
        for succ in graph.successors(&current) {
            if visited.insert(succ.id.clone()) {
                result.push(succ.id.clone());
                stack.push(succ.id.clone());
            }
        }
    }
    result
}

fn all_inputs_terminal(graph: &Graph, node_id: &NodeId, ctx: &ExecutionContext) -> bool {
    graph
        .predecessors(node_id)
        .iter()
        .all(|pred| ctx.get_state(&pred.id.0).is_terminal())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::sync_handler;
    use crate::execute::Outputs;
    use crate::graph::node::Node;
    use crate::graph::types::Value;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    // -- Basic event-driven tests --

    #[tokio::test]
    async fn event_fires_entry_node() {
        let mut g = Graph::new();
        g.add_node(Node::new("ENTRY", "Entry").with_handler("echo"))
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("echo".into(), sync_handler(|_, inputs| Ok(inputs)));

        let (executor, sender) = EventDrivenExecutor::new();
        let token = executor.cancel_token().clone();

        let exec_handle = tokio::spawn(async move { executor.execute(&g, &handlers).await });

        // Send an event
        let mut data = Outputs::new();
        data.insert("message".into(), Value::String("hello".into()));
        sender
            .send(EventMessage {
                target_node: "ENTRY".into(),
                data,
            })
            .await
            .unwrap();

        // Give it time to process, then cancel
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();

        let result = exec_handle.await.unwrap().unwrap();
        assert_eq!(result.node_states["ENTRY"], NodeState::Completed);
        assert_eq!(
            result.node_outputs["ENTRY"]["message"],
            Value::String("hello".into())
        );
    }

    #[tokio::test]
    async fn event_propagates_downstream() {
        // ENTRY → PROCESS → SINK
        let mut g = Graph::new();
        g.add_node(Node::new("ENTRY", "Entry").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("PROCESS", "Process").with_handler("transform"))
            .unwrap();
        g.add_node(Node::new("SINK", "Sink").with_handler("pass"))
            .unwrap();
        g.add_edge(&"ENTRY".into(), "value", &"PROCESS".into(), "input", None)
            .unwrap();
        g.add_edge(&"PROCESS".into(), "output", &"SINK".into(), "result", None)
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "transform".into(),
            sync_handler(|_, inputs| {
                let val = match inputs.get("input") {
                    Some(Value::I64(n)) => *n,
                    _ => 0,
                };
                let mut out = Outputs::new();
                out.insert("output".into(), Value::I64(val * 2));
                Ok(out)
            }),
        );

        let (executor, sender) = EventDrivenExecutor::new();
        let token = executor.cancel_token().clone();

        let exec_handle = tokio::spawn(async move { executor.execute(&g, &handlers).await });

        let mut data = Outputs::new();
        data.insert("value".into(), Value::I64(21));
        sender
            .send(EventMessage {
                target_node: "ENTRY".into(),
                data,
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();

        let result = exec_handle.await.unwrap().unwrap();
        assert_eq!(result.node_states["ENTRY"], NodeState::Completed);
        assert_eq!(result.node_states["PROCESS"], NodeState::Completed);
        assert_eq!(result.node_states["SINK"], NodeState::Completed);
        assert_eq!(result.node_outputs["PROCESS"]["output"], Value::I64(42));
    }

    #[tokio::test]
    async fn multiple_events_processed() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let mut g = Graph::new();
        g.add_node(Node::new("ENTRY", "Entry").with_handler("count"))
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "count".into(),
            sync_handler(move |_, inputs| {
                counter_clone.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(inputs)
            }),
        );

        let (executor, sender) = EventDrivenExecutor::new();
        let token = executor.cancel_token().clone();

        let exec_handle = tokio::spawn(async move { executor.execute(&g, &handlers).await });

        // Send 3 events
        for i in 0..3 {
            let mut data = Outputs::new();
            data.insert("seq".into(), Value::I64(i));
            sender
                .send(EventMessage {
                    target_node: "ENTRY".into(),
                    data,
                })
                .await
                .unwrap();
            // Small delay to let each event process
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();

        let _result = exec_handle.await.unwrap().unwrap();
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 3);
    }

    #[tokio::test]
    async fn channel_close_stops_executor() {
        let mut g = Graph::new();
        g.add_node(Node::new("ENTRY", "Entry")).unwrap();

        let (executor, sender) = EventDrivenExecutor::new();

        let exec_handle =
            tokio::spawn(async move { executor.execute(&g, &HandlerRegistry::new()).await });

        // Drop the sender to close the channel
        drop(sender);

        // Executor should exit cleanly
        let result = exec_handle.await.unwrap().unwrap();
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, ExecutionEvent::ExecutionCompleted { .. })));
    }

    #[tokio::test]
    async fn event_to_nonexistent_node_emits_error() {
        let mut g = Graph::new();
        g.add_node(Node::new("ENTRY", "Entry")).unwrap();

        let (executor, sender) = EventDrivenExecutor::new();
        let token = executor.cancel_token().clone();

        let exec_handle = tokio::spawn(async move { executor.execute(&g, &handlers()).await });

        // Send event to non-existent node
        sender
            .send(EventMessage {
                target_node: "MISSING".into(),
                data: Outputs::new(),
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();

        let result = exec_handle.await.unwrap().unwrap();
        // Should have a NodeFailed event for the missing node
        assert!(result.events.iter().any(|e| matches!(
            e,
            ExecutionEvent::NodeFailed { node_id, .. } if node_id == "MISSING"
        )));
    }

    #[tokio::test]
    async fn cancellation_stops_event_loop() {
        let mut g = Graph::new();
        g.add_node(Node::new("ENTRY", "Entry")).unwrap();

        let token = CancellationToken::new();
        let (executor, _sender) = EventDrivenExecutor::with_cancel(token.clone());

        let exec_handle =
            tokio::spawn(async move { executor.execute(&g, &HandlerRegistry::new()).await });

        // Cancel immediately
        token.cancel();

        let result = exec_handle.await.unwrap().unwrap();
        assert!(result
            .events
            .iter()
            .any(|e| matches!(e, ExecutionEvent::ExecutionCompleted { .. })));
    }

    #[tokio::test]
    async fn repeated_events_with_downstream_re_execute() {
        // ENTRY → PROCESS: send two events, both must propagate through PROCESS
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let mut g = Graph::new();
        g.add_node(Node::new("ENTRY", "Entry").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("PROCESS", "Process").with_handler("count"))
            .unwrap();
        g.add_edge(&"ENTRY".into(), "", &"PROCESS".into(), "", None)
            .unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "count".into(),
            sync_handler(move |_, inputs| {
                counter_clone.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(inputs)
            }),
        );

        let (executor, sender) = EventDrivenExecutor::new();
        let token = executor.cancel_token().clone();

        let exec_handle = tokio::spawn(async move { executor.execute(&g, &handlers).await });

        // Send first event
        sender
            .send(EventMessage {
                target_node: "ENTRY".into(),
                data: Outputs::new(),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Send second event — downstream PROCESS must re-execute
        sender
            .send(EventMessage {
                target_node: "ENTRY".into(),
                data: Outputs::new(),
            })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;

        token.cancel();
        let _result = exec_handle.await.unwrap().unwrap();

        // PROCESS should have executed twice (once per event)
        assert_eq!(counter.load(AtomicOrdering::SeqCst), 2);
    }

    #[tokio::test]
    async fn event_targets_mid_graph_node() {
        // A → B → C: event targets B (mid-graph), should execute B and C
        // A is never triggered
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("B", "B").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("C", "C").with_handler("pass"))
            .unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"C".into(), "", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));

        let (executor, sender) = EventDrivenExecutor::new();
        let token = executor.cancel_token().clone();

        let exec_handle = tokio::spawn(async move { executor.execute(&g, &handlers).await });

        let mut data = Outputs::new();
        data.insert("value".into(), Value::I64(99));
        sender
            .send(EventMessage {
                target_node: "B".into(),
                data,
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();

        let result = exec_handle.await.unwrap().unwrap();
        // B and C should complete; A was never triggered (gets cancelled on shutdown)
        assert_eq!(result.node_states["B"], NodeState::Completed);
        assert_eq!(result.node_states["C"], NodeState::Completed);
        // A was Idle when the cancel token fired, so it transitions to Cancelled
        assert_eq!(result.node_states["A"], NodeState::Cancelled);
    }

    fn handlers() -> HandlerRegistry {
        HandlerRegistry::new()
    }
}
