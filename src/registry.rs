use crate::execute::{ExecutionContext, HandlerRegistry, NodeHandler};
use crate::graph::Graph;
use crate::scripting::engine::ScriptEngine;
use std::collections::HashMap;
use std::sync::Arc;

/// Registry for node handler implementations, keyed by handler name.
///
/// Provides registration, lookup, override, and bulk registration.
/// The executor resolves handler names from node annotations against this registry.
pub struct NodeRegistry {
    handlers: HashMap<String, Arc<dyn NodeHandler>>,
}

impl NodeRegistry {
    pub fn new() -> Self {
        Self {
            handlers: HashMap::new(),
        }
    }

    /// Create a registry pre-populated with all built-in standalone handlers,
    /// including the `"rhai"` script handler backed by the given engine.
    ///
    /// Does not include wrapper handlers (`catch`, `fallback`, `retry`) or
    /// context-dependent handlers (`accumulator`, `break`, `select`,
    /// `subgraph_invoke`, `human_input`, `llm_call`) since those require
    /// runtime arguments. See [`NodeRegistry::with_defaults_full`] for a
    /// registry that includes handlers needing an [`ExecutionContext`].
    pub fn with_defaults(engine: Arc<ScriptEngine>) -> Self {
        use crate::handlers::*;

        let mut reg = Self::new();

        // Utility handlers
        reg.register(
            "passthrough",
            Arc::new(PassthroughHandler) as Arc<dyn NodeHandler>,
        );
        reg.register("transform", Arc::new(TransformHandler));
        reg.register("delay", Arc::new(DelayHandler));
        reg.register("log", Arc::new(LogHandler));
        reg.register("merge", Arc::new(MergeHandler));
        reg.register("split", Arc::new(SplitHandler));
        reg.register("gate", Arc::new(GateHandler));
        reg.register("error_transform", Arc::new(ErrorTransformHandler));

        // Integration handlers
        reg.register("http", Arc::new(HttpHandler));
        reg.register("read_file", Arc::new(ReadFileHandler));
        reg.register("write_file", Arc::new(WriteFileHandler));
        reg.register("glob", Arc::new(GlobHandler));

        // Scripting
        reg.register("rhai", Arc::new(RhaiHandler::new(engine)));

        reg
    }

    /// Create a registry with the stateless defaults from [`with_defaults`]
    /// plus context-dependent handlers that need [`ExecutionContext`] for
    /// blackboard access.
    ///
    /// Adds: `accumulator`, `break`, `select`.
    ///
    /// Does NOT add handlers whose construction requires resources beyond an
    /// `ExecutionContext`:
    ///
    /// - `llm_call` — requires an `AiAdapter`; embedders pick their own.
    /// - `human_input` — returns a receiver that must be owned by the operator
    ///   loop; embedders call `HumanInputHandler::new()` themselves.
    /// - `retry` / `catch` / `fallback` — wrap a specific inner handler and
    ///   are constructed per-node by the embedder.
    /// - `subgraph_invoke` — requires a `GraphLibrary` and a deferred
    ///   `HandlerRegistrySlot`.
    ///
    /// Embedders layer those on top of the returned registry.
    pub fn with_defaults_full(engine: Arc<ScriptEngine>, ctx: Arc<ExecutionContext>) -> Self {
        use crate::handlers::*;

        let mut reg = Self::with_defaults(engine);

        reg.register(
            "accumulator",
            Arc::new(AccumulatorHandler::new(ctx.clone())) as Arc<dyn NodeHandler>,
        );
        reg.register("break", Arc::new(BreakHandler::new(ctx.clone())));
        reg.register("select", Arc::new(SelectHandler::new(ctx)));

        reg
    }

    /// Register a handler by name. Overwrites any existing handler with the same name.
    pub fn register(&mut self, name: impl Into<String>, handler: Arc<dyn NodeHandler>) {
        self.handlers.insert(name.into(), handler);
    }

    /// Look up a handler by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn NodeHandler>> {
        self.handlers.get(name).cloned()
    }

    /// Check whether a handler name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }

    /// List all registered handler names.
    pub fn names(&self) -> Vec<&str> {
        self.handlers.keys().map(|k| k.as_str()).collect()
    }

    /// Remove a handler by name. Returns the removed handler, if any.
    pub fn remove(&mut self, name: &str) -> Option<Arc<dyn NodeHandler>> {
        self.handlers.remove(name)
    }

    /// Convert into the `HandlerRegistry` type expected by the executor.
    pub fn into_handler_registry(self) -> HandlerRegistry {
        self.handlers
    }

    /// Borrow as a `HandlerRegistry` reference for execution.
    pub fn as_handler_registry(&self) -> &HandlerRegistry {
        &self.handlers
    }

    /// Validate that all handler names referenced in the graph are registered.
    /// Returns a list of (node_id, handler_name) pairs for missing handlers.
    pub fn validate_graph(&self, graph: &Graph) -> Vec<(String, String)> {
        let mut missing = Vec::new();
        for node in graph.nodes() {
            if let Some(ref handler_name) = node.handler {
                if !self.handlers.contains_key(handler_name) {
                    missing.push((node.id.0.clone(), handler_name.clone()));
                }
            }
        }
        missing
    }
}

impl Default for NodeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl From<HandlerRegistry> for NodeRegistry {
    fn from(handlers: HandlerRegistry) -> Self {
        Self { handlers }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::sync_handler;
    use crate::execute::Outputs;
    use crate::graph::node::Node;

    #[test]
    fn register_and_lookup() {
        let mut reg = NodeRegistry::new();
        reg.register("test", sync_handler(|_, inputs| Ok(inputs)));
        assert!(reg.contains("test"));
        assert!(reg.get("test").is_some());
        assert!(!reg.contains("missing"));
        assert!(reg.get("missing").is_none());
    }

    #[test]
    fn override_handler() {
        let mut reg = NodeRegistry::new();
        reg.register("h", sync_handler(|_, _| Ok(Outputs::new())));
        reg.register("h", sync_handler(|_, _| Ok(Outputs::new())));
        assert!(reg.contains("h"));
        assert_eq!(reg.names().len(), 1);
    }

    #[test]
    fn remove_handler() {
        let mut reg = NodeRegistry::new();
        reg.register("h", sync_handler(|_, _| Ok(Outputs::new())));
        assert!(reg.remove("h").is_some());
        assert!(!reg.contains("h"));
        assert!(reg.remove("h").is_none());
    }

    #[test]
    fn list_names() {
        let mut reg = NodeRegistry::new();
        reg.register("alpha", sync_handler(|_, inputs| Ok(inputs)));
        reg.register("beta", sync_handler(|_, inputs| Ok(inputs)));
        let mut names = reg.names();
        names.sort();
        assert_eq!(names, vec!["alpha", "beta"]);
    }

    #[test]
    fn validate_graph_finds_missing() {
        let mut graph = Graph::new();
        graph
            .add_node(Node::new("A", "A").with_handler("exists"))
            .unwrap();
        graph
            .add_node(Node::new("B", "B").with_handler("missing"))
            .unwrap();
        graph.add_node(Node::new("C", "C")).unwrap(); // no handler

        let mut reg = NodeRegistry::new();
        reg.register("exists", sync_handler(|_, inputs| Ok(inputs)));

        let missing = reg.validate_graph(&graph);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], ("B".to_string(), "missing".to_string()));
    }

    #[test]
    fn validate_graph_all_present() {
        let mut graph = Graph::new();
        graph
            .add_node(Node::new("A", "A").with_handler("h"))
            .unwrap();

        let mut reg = NodeRegistry::new();
        reg.register("h", sync_handler(|_, inputs| Ok(inputs)));

        assert!(reg.validate_graph(&graph).is_empty());
    }

    #[test]
    fn into_handler_registry() {
        let mut reg = NodeRegistry::new();
        reg.register("h", sync_handler(|_, inputs| Ok(inputs)));
        let hr = reg.into_handler_registry();
        assert!(hr.contains_key("h"));
    }

    #[test]
    fn with_defaults_registers_stateless_handlers() {
        let engine = Arc::new(ScriptEngine::with_defaults());
        let reg = NodeRegistry::with_defaults(engine);

        for name in [
            "passthrough",
            "transform",
            "delay",
            "log",
            "merge",
            "split",
            "gate",
            "error_transform",
            "http",
            "read_file",
            "write_file",
            "glob",
            "rhai",
        ] {
            assert!(reg.contains(name), "missing handler: {name}");
        }
    }

    #[test]
    fn with_defaults_full_adds_context_handlers() {
        let engine = Arc::new(ScriptEngine::with_defaults());
        let ctx = Arc::new(ExecutionContext::new());
        let reg = NodeRegistry::with_defaults_full(engine, ctx);

        // All the stateless defaults are still there
        assert!(reg.contains("passthrough"));
        assert!(reg.contains("gate"));
        assert!(reg.contains("rhai"));

        // Plus the context-dependent ones
        assert!(reg.contains("accumulator"));
        assert!(reg.contains("break"));
        assert!(reg.contains("select"));
    }
}
