use crate::execute::{HandlerRegistry, NodeHandler};
use crate::graph::Graph;
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
}
