use crate::execute::{ExecutionContext, HandlerRegistry, HandlerSchema, NodeHandler};
use crate::graph::Graph;
use crate::scripting::engine::ScriptEngine;
use crate::template::TemplateResolver;
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
    ///
    /// The returned registry uses psflow's default
    /// [`crate::template::PromptTemplateResolver`] for handlers that resolve
    /// templates via [`crate::template::TemplateResolver`]. Embedders wanting
    /// a richer template engine should use
    /// [`NodeRegistry::with_defaults_and_resolver`] instead.
    pub fn with_defaults(engine: Arc<ScriptEngine>) -> Self {
        Self::with_defaults_and_resolver(engine, crate::template::default_resolver())
    }

    /// Same as [`with_defaults`], but lets the embedder supply its own
    /// [`TemplateResolver`] so built-in handlers that accept templated
    /// config (e.g. `shell`) resolve them through the embedder's engine.
    pub fn with_defaults_and_resolver(
        engine: Arc<ScriptEngine>,
        resolver: Arc<dyn TemplateResolver>,
    ) -> Self {
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
        reg.register("http", Arc::new(HttpHandler::stateless()));
        reg.register(WS_HANDLER_NAME, Arc::new(WebSocketHandler::stateless()));
        reg.register("read_file", Arc::new(ReadFileHandler));
        reg.register("write_file", Arc::new(WriteFileHandler));
        reg.register("glob", Arc::new(GlobHandler));
        reg.register("shell", Arc::new(ShellHandler::new(resolver.clone())));
        reg.register("json_transform", Arc::new(JsonTransformHandler));

        // Scripting
        reg.register("rhai", Arc::new(RhaiHandler::new(engine)));

        // The resolver argument is held by handlers that need it; no direct
        // registry-level storage is necessary.
        let _ = resolver;

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
    ///
    /// Uses psflow's default [`crate::template::PromptTemplateResolver`] for
    /// templated-config built-ins. See
    /// [`NodeRegistry::with_defaults_full_and_resolver`] for the variant that
    /// takes an embedder-supplied resolver.
    pub fn with_defaults_full(engine: Arc<ScriptEngine>, ctx: Arc<ExecutionContext>) -> Self {
        Self::with_defaults_full_and_resolver(engine, ctx, crate::template::default_resolver())
    }

    /// Full defaults with an explicit [`TemplateResolver`] — the composition
    /// of [`with_defaults_and_resolver`] and the context-dependent handlers
    /// from [`with_defaults_full`].
    pub fn with_defaults_full_and_resolver(
        engine: Arc<ScriptEngine>,
        ctx: Arc<ExecutionContext>,
        resolver: Arc<dyn TemplateResolver>,
    ) -> Self {
        use crate::handlers::*;

        let mut reg = Self::with_defaults_and_resolver(engine, resolver);

        reg.register(
            "accumulator",
            Arc::new(AccumulatorHandler::new(ctx.clone())) as Arc<dyn NodeHandler>,
        );
        reg.register("break", Arc::new(BreakHandler::new(ctx.clone())));
        reg.register("select", Arc::new(SelectHandler::new(ctx.clone())));
        // Override the stateless HTTP + WS handlers with context-bound ones so
        // that `config.auth` can resolve against the graph's auth registry.
        reg.register("http", Arc::new(HttpHandler::new(ctx.clone())));
        reg.register(WS_HANDLER_NAME, Arc::new(WebSocketHandler::new(ctx)));

        reg
    }

    /// Emit a manifest of schema metadata for every registered handler.
    ///
    /// Walks the registry, calls [`NodeHandler::schema`] on each handler, and
    /// returns the result as a JSON value shaped as:
    ///
    /// ```json
    /// {
    ///   "handlers": [
    ///     { "name": "...", "description": "...", "config": [...], ... },
    ///     ...
    ///   ]
    /// }
    /// ```
    ///
    /// Consumed by the `psflow-manifest` binary (and downstream tooling like
    /// Ergon's MCP handler catalogue).
    pub fn manifest(&self) -> serde_json::Value {
        let mut schemas: Vec<HandlerSchema> = self
            .handlers
            .iter()
            .map(|(name, handler)| handler.schema(name))
            .collect();
        schemas.sort_by(|a, b| a.name.cmp(&b.name));
        serde_json::json!({ "handlers": schemas })
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
    use crate::handlers::websocket::WS_HANDLER_NAME;

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
            WS_HANDLER_NAME,
            "read_file",
            "write_file",
            "glob",
            "rhai",
            "shell",
            "json_transform",
        ] {
            assert!(reg.contains(name), "missing handler: {name}");
        }
    }

    #[test]
    fn manifest_lists_all_registered_handlers() {
        let engine = Arc::new(ScriptEngine::with_defaults());
        let reg = NodeRegistry::with_defaults(engine);
        let manifest = reg.manifest();
        let handlers = manifest.get("handlers").and_then(|v| v.as_array()).unwrap();
        let names: std::collections::HashSet<String> = handlers
            .iter()
            .filter_map(|h| h.get("name").and_then(|n| n.as_str()).map(str::to_owned))
            .collect();
        assert!(names.contains("shell"));
        assert!(names.contains("json_transform"));
        assert!(names.contains("http"));
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
