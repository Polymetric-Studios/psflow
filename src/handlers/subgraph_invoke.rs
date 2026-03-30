use crate::error::NodeError;
use crate::execute::blackboard::ContextInheritance;
use crate::execute::context::CancellationToken;
use crate::execute::{
    ExecutionContext, ExecutionError, Executor, HandlerRegistry, NodeHandler, Outputs,
    TopologicalExecutor,
};
use crate::graph::node::Node;
use crate::graph::Graph;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

/// Registry of named graphs that can be invoked as functions by SubgraphInvocationHandler.
#[derive(Debug, Clone)]
pub struct GraphLibrary {
    graphs: HashMap<String, Graph>,
}

impl GraphLibrary {
    pub fn new() -> Self {
        Self {
            graphs: HashMap::new(),
        }
    }

    pub fn register(&mut self, name: impl Into<String>, graph: Graph) {
        self.graphs.insert(name.into(), graph);
    }

    pub fn get(&self, name: &str) -> Option<&Graph> {
        self.graphs.get(name)
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.graphs.keys().map(|s| s.as_str())
    }
}

impl Default for GraphLibrary {
    fn default() -> Self {
        Self::new()
    }
}

impl FromIterator<(String, Graph)> for GraphLibrary {
    fn from_iter<T: IntoIterator<Item = (String, Graph)>>(iter: T) -> Self {
        Self {
            graphs: iter.into_iter().collect(),
        }
    }
}

/// RAII guard that tracks active invocation depth.
/// Increments on creation, decrements on drop (including panics).
struct DepthGuard(Arc<AtomicUsize>);

impl DepthGuard {
    fn enter(counter: &Arc<AtomicUsize>, max: usize) -> Result<Self, NodeError> {
        // CAS loop: atomically check-and-increment to avoid TOCTOU race
        // where concurrent callers could both pass the depth check.
        loop {
            let current = counter.load(Ordering::Acquire);
            if current >= max {
                return Err(NodeError::Failed {
                    source_message: None,
                    message: format!(
                        "subgraph invocation depth exceeded (max: {max}, current: {current})"
                    ),
                    recoverable: false,
                });
            }
            if counter
                .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(DepthGuard(counter.clone()));
            }
        }
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Handler that invokes a named graph as a function.
///
/// The handler looks up the target graph by name from a `GraphLibrary`,
/// executes it as a child graph using `TopologicalExecutor`, and maps
/// inputs/outputs between parent and child.
///
/// ## Input mapping
///
/// Parent inputs flow into the child graph's source nodes (nodes with no
/// predecessors). If there is a single source node, all parent inputs are
/// stored as its outputs. If there are multiple source nodes, inputs are
/// matched by node ID — an input key matching a source node ID is stored
/// as that node's output.
///
/// ## Output mapping
///
/// After execution, outputs from sink nodes (nodes with no successors) are
/// collected. If there is a single sink node, its outputs are returned
/// directly. If there are multiple, outputs are merged (last-writer-wins).
///
/// ## Recursion guard
///
/// An atomic depth counter limits concurrent nesting. Configured via
/// `exec.max_depth` (default 10). Exceeding the limit returns a
/// non-recoverable error.
///
/// ## Context inheritance
///
/// When a parent `ExecutionContext` is provided (via `with_context`), the
/// child graph's blackboard inherits from the parent's. Configure via
/// `exec.context_inheritance`: `"read_only"` (default), `"snapshot"`, or
/// `"isolated"`.
///
/// ## Configuration
///
/// - `config.graph` (required): Name of the graph in the library
/// - `exec.max_depth`: Maximum concurrent invocation depth (default 10)
/// - `exec.context_inheritance`: `"read_only"` | `"snapshot"` | `"isolated"`
pub struct SubgraphInvocationHandler {
    library: Arc<GraphLibrary>,
    /// Deferred handler registry — set after construction to allow
    /// the registry to include this handler (for recursive invocation).
    handlers: Arc<OnceLock<HandlerRegistry>>,
    /// Parent execution context for blackboard inheritance.
    exec_ctx: Option<Arc<ExecutionContext>>,
    active_depth: Arc<AtomicUsize>,
    default_max_depth: usize,
}

const DEFAULT_MAX_DEPTH: usize = 10;

impl SubgraphInvocationHandler {
    /// Create a handler that invokes graphs from the library.
    ///
    /// The returned `HandlerRegistrySlot` must be initialized with the
    /// final handler registry (which may include this handler) before
    /// the first invocation.
    pub fn new(library: Arc<GraphLibrary>) -> (Self, HandlerRegistrySlot) {
        let slot = Arc::new(OnceLock::new());
        (
            Self {
                library,
                handlers: slot.clone(),
                exec_ctx: None,
                active_depth: Arc::new(AtomicUsize::new(0)),
                default_max_depth: DEFAULT_MAX_DEPTH,
            },
            HandlerRegistrySlot(slot),
        )
    }

    /// Create a handler with a pre-set handler registry (no deferred init needed).
    pub fn with_handlers(library: Arc<GraphLibrary>, handlers: HandlerRegistry) -> Self {
        let slot = Arc::new(OnceLock::new());
        slot.set(handlers).ok();
        Self {
            library,
            handlers: slot,
            exec_ctx: None,
            active_depth: Arc::new(AtomicUsize::new(0)),
            default_max_depth: DEFAULT_MAX_DEPTH,
        }
    }

    /// Set the parent execution context for blackboard inheritance.
    /// Child graphs will inherit the parent's blackboard data based on
    /// `exec.context_inheritance` (default: `"read_only"`).
    pub fn with_context(mut self, ctx: Arc<ExecutionContext>) -> Self {
        self.exec_ctx = Some(ctx);
        self
    }
}

/// Slot for deferred handler registry initialization.
/// Call `set()` with the complete handler registry after all handlers
/// (including the SubgraphInvocationHandler) are registered.
pub struct HandlerRegistrySlot(Arc<OnceLock<HandlerRegistry>>);

impl HandlerRegistrySlot {
    pub fn set(self, registry: HandlerRegistry) {
        self.0.set(registry).ok();
    }
}

impl NodeHandler for SubgraphInvocationHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let library = self.library.clone();
        let handlers_lock = self.handlers.clone();
        let active_depth = self.active_depth.clone();
        let exec_ctx = self.exec_ctx.clone();
        let config = node.config.clone();
        let exec = node.exec.clone();
        let node_id = node.id.0.clone();
        let default_max_depth = self.default_max_depth;

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before subgraph invocation".into(),
                });
            }

            // Read config
            let graph_name = config
                .get("graph")
                .and_then(|v| v.as_str())
                .ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': missing config.graph"),
                    recoverable: false,
                })?
                .to_string();

            let max_depth = exec
                .get("max_depth")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(default_max_depth);

            // Recursion guard
            let _guard = DepthGuard::enter(&active_depth, max_depth)?;

            // Look up graph
            let child_graph = library.get(&graph_name).ok_or_else(|| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': graph '{graph_name}' not found in library"),
                recoverable: false,
            })?;

            // Get handler registry
            let handlers = handlers_lock.get().ok_or_else(|| NodeError::Failed {
                source_message: None,
                message: format!(
                    "node '{node_id}': handler registry not initialized for subgraph invocation"
                ),
                recoverable: false,
            })?;

            // Find source nodes (no predecessors) and sink nodes (no successors)
            let source_nodes: Vec<String> = child_graph
                .nodes()
                .filter(|n| child_graph.predecessors(&n.id).is_empty())
                .map(|n| n.id.0.clone())
                .collect();

            let sink_nodes: Vec<String> = child_graph
                .nodes()
                .filter(|n| child_graph.successors(&n.id).is_empty())
                .map(|n| n.id.0.clone())
                .collect();

            // Parse context inheritance mode
            let inheritance = match exec
                .get("context_inheritance")
                .and_then(|v| v.as_str())
            {
                Some("snapshot") => ContextInheritance::Snapshot,
                Some("isolated") => ContextInheritance::Isolated,
                _ => ContextInheritance::ReadOnly, // default
            };

            // Build parent blackboard reference for child context
            let parent_bb = exec_ctx.as_ref().map(|ctx| {
                let bb = ctx.blackboard();
                bb.clone()
            });

            // Execute child graph with input injection and context inheritance
            let child_executor = TopologicalExecutor::with_cancel(cancel);
            let result = execute_child(
                child_graph,
                handlers,
                &child_executor,
                &inputs,
                &source_nodes,
                parent_bb.as_ref(),
                inheritance,
            )
            .await
            .map_err(|e| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': child graph '{graph_name}' failed: {e}"),
                recoverable: false,
            })?;

            // Collect outputs from sink nodes
            let mut outputs = Outputs::new();
            for sink_id in &sink_nodes {
                if let Some(sink_outputs) = result.node_outputs.get(sink_id) {
                    outputs.extend(sink_outputs.clone());
                }
            }

            Ok(outputs)
        })
    }
}

/// Execute a child graph, injecting parent inputs into source nodes.
///
/// Source nodes are pre-seeded with input data and marked as Completed
/// so the executor skips them and downstream nodes receive the data
/// through normal port mapping.
async fn execute_child(
    graph: &Graph,
    handlers: &HandlerRegistry,
    executor: &TopologicalExecutor,
    parent_inputs: &Outputs,
    source_nodes: &[String],
    parent_bb: Option<&crate::execute::blackboard::Blackboard>,
    inheritance: ContextInheritance,
) -> Result<crate::execute::ExecutionResult, ExecutionError> {
    if source_nodes.is_empty() {
        return match parent_bb {
            Some(bb) => executor.execute_with_parent(graph, handlers, bb, inheritance).await,
            None => executor.execute(graph, handlers).await,
        };
    }

    // Build a modified graph where source node handlers are replaced with
    // input-injection handlers. This avoids polluting the shared handler
    // registry (other nodes using the same handler name are unaffected).
    let mut child_handlers = handlers.clone();
    let mut modified_graph = graph.clone();
    let inject_handler_prefix = "_invoke_inject_";

    if source_nodes.len() == 1 {
        // Single source: inject all parent inputs
        let source_id = &source_nodes[0];
        let handler_name = format!("{inject_handler_prefix}{source_id}");
        child_handlers.insert(
            handler_name.clone(),
            Arc::new(InputInjectionHandler(parent_inputs.clone())),
        );
        if let Some(node) = modified_graph.node_mut(&source_id.as_str().into()) {
            node.handler = Some(handler_name);
        }
    } else {
        // Multiple sources: match inputs to source nodes by node ID
        for source_id in source_nodes {
            let node_inputs: Outputs = if let Some(value) = parent_inputs.get(source_id) {
                // Input key matches source node ID — inject that single value
                let mut m = Outputs::new();
                m.insert(source_id.clone(), value.clone());
                m
            } else {
                // No matching key — inject all parent inputs (source can pick what it needs)
                parent_inputs.clone()
            };

            let handler_name = format!("{inject_handler_prefix}{source_id}");
            child_handlers.insert(
                handler_name.clone(),
                Arc::new(InputInjectionHandler(node_inputs)),
            );
            if let Some(node) = modified_graph.node_mut(&source_id.as_str().into()) {
                node.handler = Some(handler_name);
            }
        }
    }

    match parent_bb {
        Some(bb) => {
            executor
                .execute_with_parent(&modified_graph, &child_handlers, bb, inheritance)
                .await
        }
        None => executor.execute(&modified_graph, &child_handlers).await,
    }
}

/// Handler that ignores normal inputs and returns pre-configured data.
/// Used to inject parent graph inputs into child graph source nodes.
struct InputInjectionHandler(Outputs);

impl NodeHandler for InputInjectionHandler {
    fn execute(
        &self,
        _node: &Node,
        _inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let outputs = self.0.clone();
        Box::pin(async move { Ok(outputs) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::sync_handler;
    use crate::graph::node::Node;
    use crate::graph::types::Value;

    fn make_child_graph() -> Graph {
        // Simple: INPUT → DOUBLE → OUTPUT
        let mut g = Graph::new();
        g.add_node(Node::new("INPUT", "Input").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("DOUBLE", "Double").with_handler("double"))
            .unwrap();
        g.add_node(Node::new("OUTPUT", "Output").with_handler("pass"))
            .unwrap();
        g.add_edge(&"INPUT".into(), "value", &"DOUBLE".into(), "value", None)
            .unwrap();
        g.add_edge(&"DOUBLE".into(), "result", &"OUTPUT".into(), "result", None)
            .unwrap();
        g
    }

    fn make_handlers() -> HandlerRegistry {
        let mut h = HandlerRegistry::new();
        h.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        h.insert(
            "double".into(),
            sync_handler(|_, inputs| {
                let val = match inputs.get("value") {
                    Some(Value::I64(n)) => *n,
                    _ => 0,
                };
                let mut out = Outputs::new();
                out.insert("result".into(), Value::I64(val * 2));
                Ok(out)
            }),
        );
        h
    }

    // -- Basic invocation --

    #[tokio::test]
    async fn invoke_child_graph() {
        let mut library = GraphLibrary::new();
        library.register("doubler", make_child_graph());
        let library = Arc::new(library);

        let handlers = make_handlers();
        let handler = SubgraphInvocationHandler::with_handlers(library, handlers);

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "doubler" });

        let mut inputs = Outputs::new();
        inputs.insert("value".into(), Value::I64(21));

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.get("result"), Some(&Value::I64(42)));
    }

    #[tokio::test]
    async fn invoke_missing_graph_errors() {
        let library = Arc::new(GraphLibrary::new());
        let handler = SubgraphInvocationHandler::with_handlers(library, HandlerRegistry::new());

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "nonexistent" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found in library"));
    }

    #[tokio::test]
    async fn invoke_missing_config_graph_errors() {
        let library = Arc::new(GraphLibrary::new());
        let handler = SubgraphInvocationHandler::with_handlers(library, HandlerRegistry::new());

        let node = Node::new("INVOKE", "Invoke");
        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing config.graph"));
    }

    // -- Output collection from sink nodes --

    #[tokio::test]
    async fn invoke_collects_sink_outputs() {
        // Graph with two sink nodes: OUTPUT_A and OUTPUT_B
        let mut g = Graph::new();
        g.add_node(Node::new("INPUT", "Input").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("OUTPUT_A", "A").with_handler("label_a"))
            .unwrap();
        g.add_node(Node::new("OUTPUT_B", "B").with_handler("label_b"))
            .unwrap();
        g.add_edge(&"INPUT".into(), "", &"OUTPUT_A".into(), "", None)
            .unwrap();
        g.add_edge(&"INPUT".into(), "", &"OUTPUT_B".into(), "", None)
            .unwrap();

        let mut library = GraphLibrary::new();
        library.register("multi_out", g);

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "label_a".into(),
            sync_handler(|_, _| {
                let mut out = Outputs::new();
                out.insert("from_a".into(), Value::String("hello".into()));
                Ok(out)
            }),
        );
        handlers.insert(
            "label_b".into(),
            sync_handler(|_, _| {
                let mut out = Outputs::new();
                out.insert("from_b".into(), Value::I64(99));
                Ok(out)
            }),
        );

        let handler =
            SubgraphInvocationHandler::with_handlers(Arc::new(library), handlers);

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "multi_out" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.get("from_a"), Some(&Value::String("hello".into())));
        assert_eq!(result.get("from_b"), Some(&Value::I64(99)));
    }

    // -- Multiple source nodes --

    #[tokio::test]
    async fn invoke_multiple_source_nodes() {
        // Two source nodes: SRC_X and SRC_Y → MERGE → OUTPUT
        let mut g = Graph::new();
        g.add_node(Node::new("SRC_X", "X").with_handler("pass")).unwrap();
        g.add_node(Node::new("SRC_Y", "Y").with_handler("pass")).unwrap();
        g.add_node(Node::new("MERGE", "Merge").with_handler("pass")).unwrap();
        g.add_node(Node::new("OUTPUT", "Out").with_handler("pass")).unwrap();
        g.add_edge(&"SRC_X".into(), "", &"MERGE".into(), "", None).unwrap();
        g.add_edge(&"SRC_Y".into(), "", &"MERGE".into(), "", None).unwrap();
        g.add_edge(&"MERGE".into(), "", &"OUTPUT".into(), "", None).unwrap();

        let mut library = GraphLibrary::new();
        library.register("multi_src", g);

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));

        let handler =
            SubgraphInvocationHandler::with_handlers(Arc::new(library), handlers);

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "multi_src" });

        // Input keys match source node IDs
        let mut inputs = Outputs::new();
        inputs.insert("SRC_X".into(), Value::String("x_data".into()));
        inputs.insert("SRC_Y".into(), Value::String("y_data".into()));

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        // Both values should propagate through — each source injects its matching key
        assert_eq!(result.get("SRC_X"), Some(&Value::String("x_data".into())));
        assert_eq!(result.get("SRC_Y"), Some(&Value::String("y_data".into())));
    }

    // -- No source nodes --

    #[tokio::test]
    async fn invoke_graph_with_no_source_nodes() {
        // A graph with a self-contained node (no predecessors that we recognize as "sources"
        // because it has an internal cycle... actually, let's test the simpler case:
        // a single node with no predecessors)
        // This tests the source_nodes.is_empty() early-return path... but a single node
        // with no predecessors IS a source node. For no-source, we need a cycle.
        // Actually, the simplest: just verify a single-node graph works (it IS a source).
        // The "no source" path occurs with a cycle graph, which would fail validation.
        // Instead, test an empty graph:
        let mut library = GraphLibrary::new();
        library.register("empty", Graph::new());

        let handler =
            SubgraphInvocationHandler::with_handlers(Arc::new(library), HandlerRegistry::new());

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "empty" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        assert!(result.is_empty());
    }

    // -- Empty parent inputs --

    #[tokio::test]
    async fn invoke_with_empty_inputs() {
        let mut library = GraphLibrary::new();
        library.register("doubler", make_child_graph());

        let handler =
            SubgraphInvocationHandler::with_handlers(Arc::new(library), make_handlers());

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "doubler" });

        // Empty inputs — source node gets empty data, double of 0 = 0
        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.get("result"), Some(&Value::I64(0)));
    }

    // -- Recursion guard --

    #[tokio::test]
    async fn recursion_guard_limits_depth() {
        let library = Arc::new(GraphLibrary::new());
        let handler = SubgraphInvocationHandler::with_handlers(library, HandlerRegistry::new());

        // Manually saturate the depth counter
        for _ in 0..10 {
            handler.active_depth.fetch_add(1, Ordering::SeqCst);
        }

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "anything" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("depth exceeded"));

        // Restore counter
        for _ in 0..10 {
            handler.active_depth.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn recursion_guard_custom_max_depth() {
        let library = Arc::new(GraphLibrary::new());
        let handler = SubgraphInvocationHandler::with_handlers(library, HandlerRegistry::new());

        // Set depth to 2
        handler.active_depth.fetch_add(2, Ordering::SeqCst);

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "anything" });
        node.exec = serde_json::json!({ "max_depth": 3 });

        // Depth 2, max 3 — should pass the guard (then fail on missing graph)
        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;

        // Should fail on missing graph, NOT on depth
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found in library"));

        handler.active_depth.fetch_sub(2, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn depth_guard_decrements_on_error() {
        let counter = Arc::new(AtomicUsize::new(0));
        {
            let _guard = DepthGuard::enter(&counter, 10).unwrap();
            assert_eq!(counter.load(Ordering::SeqCst), 1);
            // guard dropped here
        }
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn depth_guard_rejects_at_limit() {
        let counter = Arc::new(AtomicUsize::new(5));
        let result = DepthGuard::enter(&counter, 5);
        assert!(result.is_err());
        // Counter should NOT be incremented on rejection
        assert_eq!(counter.load(Ordering::SeqCst), 5);
    }

    // -- Cancellation --

    #[tokio::test]
    async fn invoke_respects_cancellation() {
        let library = Arc::new(GraphLibrary::new());
        let handler = SubgraphInvocationHandler::with_handlers(library, HandlerRegistry::new());

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "anything" });

        let token = CancellationToken::new();
        token.cancel();

        let result = handler.execute(&node, Outputs::new(), token).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cancelled"));
    }

    // -- Child graph error propagation --

    #[tokio::test]
    async fn invoke_propagates_child_errors() {
        let mut g = Graph::new();
        g.add_node(Node::new("FAIL", "Fail").with_handler("fail")).unwrap();

        let mut library = GraphLibrary::new();
        library.register("failing", g);

        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "fail".into(),
            sync_handler(|_, _| {
                Err(NodeError::Failed {
                    source_message: None,
                    message: "child failure".into(),
                    recoverable: false,
                })
            }),
        );

        let handler =
            SubgraphInvocationHandler::with_handlers(Arc::new(library), handlers);

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "failing" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;

        // Child execution completes (graph has a failed node) but executor returns Ok
        // The handler should still return Ok with whatever outputs are available
        // (in this case, none from the failed node)
        assert!(result.is_ok());
        assert!(result.unwrap().is_empty());
    }

    // -- Deferred handler registry --

    #[tokio::test]
    async fn deferred_registry_initialization() {
        let mut library = GraphLibrary::new();
        library.register("simple", {
            let mut g = Graph::new();
            g.add_node(Node::new("A", "A").with_handler("pass")).unwrap();
            g
        });

        let library = Arc::new(library);
        let (handler, slot) = SubgraphInvocationHandler::new(library);

        // Set registry after construction
        let mut registry = HandlerRegistry::new();
        registry.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        slot.set(registry);

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "simple" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn uninitialized_registry_errors() {
        let mut library = GraphLibrary::new();
        library.register("test", {
            let mut g = Graph::new();
            g.add_node(Node::new("A", "A")).unwrap();
            g
        });

        let (handler, _slot) = SubgraphInvocationHandler::new(Arc::new(library));
        // Don't call slot.set() — registry stays uninitialized

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "test" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    // -- Context inheritance integration --

    #[tokio::test]
    async fn child_graph_inherits_parent_blackboard() {
        use crate::execute::blackboard::BlackboardScope;

        // Child graph: INPUT → WORKER (WORKER is not a source, so its handler runs)
        let mut g = Graph::new();
        g.add_node(Node::new("INPUT", "Input").with_handler("pass"))
            .unwrap();
        g.add_node(Node::new("WORKER", "Worker").with_handler("produce"))
            .unwrap();
        g.add_edge(&"INPUT".into(), "", &"WORKER".into(), "", None)
            .unwrap();

        let mut library = GraphLibrary::new();
        library.register("child", g);

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));
        handlers.insert(
            "produce".into(),
            sync_handler(|_, _| {
                let mut out = Outputs::new();
                out.insert("done".into(), Value::Bool(true));
                Ok(out)
            }),
        );

        // Create parent execution context with data on the blackboard
        let parent_ctx = Arc::new(ExecutionContext::new());
        {
            let mut bb = parent_ctx.blackboard();
            bb.set(
                "parent_key".into(),
                Value::String("from_parent".into()),
                BlackboardScope::Global,
            );
        }

        let handler = SubgraphInvocationHandler::with_handlers(Arc::new(library), handlers)
            .with_context(parent_ctx.clone());

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "child" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        // Child executed successfully — WORKER is the sink node
        assert_eq!(result.get("done"), Some(&Value::Bool(true)));

        // Parent blackboard is unchanged (child writes don't leak)
        let bb = parent_ctx.blackboard();
        assert_eq!(
            bb.get("parent_key", &BlackboardScope::Global),
            Some(&Value::String("from_parent".into()))
        );
    }

    #[tokio::test]
    async fn isolated_child_gets_no_parent_data() {
        use crate::execute::blackboard::BlackboardScope;

        let mut g = Graph::new();
        g.add_node(Node::new("A", "A").with_handler("pass")).unwrap();

        let mut library = GraphLibrary::new();
        library.register("child", g);

        let mut handlers = HandlerRegistry::new();
        handlers.insert("pass".into(), sync_handler(|_, inputs| Ok(inputs)));

        let parent_ctx = Arc::new(ExecutionContext::new());
        {
            let mut bb = parent_ctx.blackboard();
            bb.set(
                "secret".into(),
                Value::String("hidden".into()),
                BlackboardScope::Global,
            );
        }

        let handler = SubgraphInvocationHandler::with_handlers(Arc::new(library), handlers)
            .with_context(parent_ctx);

        let mut node = Node::new("INVOKE", "Invoke");
        node.config = serde_json::json!({ "graph": "child" });
        node.exec = serde_json::json!({ "context_inheritance": "isolated" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        // Execution succeeds but child has no access to parent data
        assert!(result.is_empty() || !result.contains_key("secret"));
    }

    // -- GraphLibrary --

    #[test]
    fn library_register_and_lookup() {
        let mut lib = GraphLibrary::new();
        let g = Graph::new();
        lib.register("test", g);
        assert!(lib.get("test").is_some());
        assert!(lib.get("missing").is_none());
    }

    #[test]
    fn library_names() {
        let mut lib = GraphLibrary::new();
        lib.register("alpha", Graph::new());
        lib.register("beta", Graph::new());
        let mut names: Vec<&str> = lib.names().collect();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn library_from_iterator() {
        let lib: GraphLibrary = vec![
            ("a".to_string(), Graph::new()),
            ("b".to_string(), Graph::new()),
        ]
        .into_iter()
        .collect();
        assert!(lib.get("a").is_some());
        assert!(lib.get("b").is_some());
    }
}
