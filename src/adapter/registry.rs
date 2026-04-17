use crate::adapter::{AdapterCapabilities, AiAdapter};
use crate::error::NodeError;
use std::collections::HashMap;
use std::sync::Arc;

/// Registry for AI adapter implementations, keyed by name.
///
/// Supports a default adapter used when nodes don't specify one explicitly.
/// Select per-graph or per-node via annotation (`%% @NODE config.adapter: "name"`).
pub struct AdapterRegistry {
    adapters: HashMap<String, Arc<dyn AiAdapter>>,
    default_name: Option<String>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self {
            adapters: HashMap::new(),
            default_name: None,
        }
    }

    /// Register an adapter by name.
    pub fn register(&mut self, adapter: Arc<dyn AiAdapter>) {
        let name = adapter.name().to_string();
        self.adapters.insert(name, adapter);
    }

    /// Register an adapter and set it as the default.
    pub fn register_default(&mut self, adapter: Arc<dyn AiAdapter>) {
        let name = adapter.name().to_string();
        self.adapters.insert(name.clone(), adapter);
        self.default_name = Some(name);
    }

    /// Set the default adapter by name. Returns error if the name isn't registered.
    pub fn set_default(&mut self, name: &str) -> Result<(), NodeError> {
        if !self.adapters.contains_key(name) {
            return Err(NodeError::AdapterError {
                adapter: name.to_string(),
                message: "adapter not registered".into(),
            });
        }
        self.default_name = Some(name.to_string());
        Ok(())
    }

    /// Look up an adapter by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn AiAdapter>> {
        self.adapters.get(name).cloned()
    }

    /// Get the default adapter.
    pub fn default_adapter(&self) -> Option<Arc<dyn AiAdapter>> {
        self.default_name
            .as_ref()
            .and_then(|name| self.adapters.get(name))
            .cloned()
    }

    /// Resolve an adapter: try the given name first, fall back to default.
    pub fn resolve(&self, name: Option<&str>) -> Result<Arc<dyn AiAdapter>, NodeError> {
        if let Some(name) = name {
            self.get(name).ok_or_else(|| NodeError::AdapterError {
                adapter: name.to_string(),
                message: "adapter not registered".into(),
            })
        } else {
            self.default_adapter()
                .ok_or_else(|| NodeError::AdapterError {
                    adapter: "default".to_string(),
                    message: "no default adapter configured".into(),
                })
        }
    }

    /// List all registered adapter names.
    pub fn names(&self) -> Vec<&str> {
        self.adapters.keys().map(|k| k.as_str()).collect()
    }

    /// Validate that an adapter satisfies the given capability requirements.
    pub fn validate_capabilities(
        &self,
        adapter_name: Option<&str>,
        required: &AdapterCapabilities,
    ) -> Result<(), NodeError> {
        let adapter = self.resolve(adapter_name)?;
        let caps = adapter.capabilities();
        if caps.satisfies(required) {
            Ok(())
        } else {
            let missing = caps.missing(required);
            Err(NodeError::AdapterError {
                adapter: adapter.name().to_string(),
                message: format!("missing capabilities: {}", missing.join(", ")),
            })
        }
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::mock::MockAdapter;
    use crate::adapter::AdapterCapabilities;

    #[test]
    fn register_and_lookup() {
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(MockAdapter::new()));
        assert!(reg.get("mock").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn register_default() {
        let mut reg = AdapterRegistry::new();
        reg.register_default(Arc::new(MockAdapter::new()));
        assert!(reg.default_adapter().is_some());
        assert_eq!(reg.default_adapter().unwrap().name(), "mock");
    }

    #[test]
    fn resolve_named() {
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(MockAdapter::new()));
        assert!(reg.resolve(Some("mock")).is_ok());
        assert!(reg.resolve(Some("missing")).is_err());
    }

    #[test]
    fn resolve_default() {
        let mut reg = AdapterRegistry::new();
        reg.register_default(Arc::new(MockAdapter::new()));
        assert!(reg.resolve(None).is_ok());
    }

    #[test]
    fn resolve_no_default_is_error() {
        let reg = AdapterRegistry::new();
        assert!(reg.resolve(None).is_err());
    }

    #[test]
    fn set_default_validates_name() {
        let mut reg = AdapterRegistry::new();
        assert!(reg.set_default("nonexistent").is_err());
        reg.register(Arc::new(MockAdapter::new()));
        assert!(reg.set_default("mock").is_ok());
    }

    #[test]
    fn validate_capabilities_passes() {
        let mut reg = AdapterRegistry::new();
        reg.register_default(Arc::new(MockAdapter::new()));

        let required = AdapterCapabilities {
            tool_use: true,
            ..Default::default()
        };
        assert!(reg.validate_capabilities(None, &required).is_ok());
    }

    #[test]
    fn validate_capabilities_fails() {
        let mut reg = AdapterRegistry::new();
        reg.register_default(Arc::new(
            MockAdapter::new().with_capabilities(AdapterCapabilities::default()),
        ));

        let required = AdapterCapabilities {
            vision: true,
            ..Default::default()
        };
        let err = reg.validate_capabilities(None, &required).unwrap_err();
        assert!(err.to_string().contains("vision"));
    }

    #[test]
    fn list_names() {
        let mut reg = AdapterRegistry::new();
        reg.register(Arc::new(MockAdapter::new()));
        assert_eq!(reg.names(), vec!["mock"]);
    }
}
