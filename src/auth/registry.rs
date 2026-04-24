use super::decl::AuthStrategyDecl;
use super::error::AuthError;
use super::strategies;
use super::strategy::AuthStrategy;
use crate::handlers::websocket::WS_HANDLER_NAME;
use std::collections::HashMap;
use std::sync::Arc;

/// Factory that produces an [`AuthStrategy`] instance from a declaration.
///
/// Factories receive the strategy's params JSON (already-interpolated params
/// are rendered later at apply time — factories see the raw template form).
/// Factories do shape validation here; they do not resolve secrets.
pub type AuthStrategyFactory =
    Arc<dyn Fn(&AuthStrategyDecl) -> Result<Arc<dyn AuthStrategy>, AuthError> + Send + Sync>;

/// Registry of strategy-type factories plus the set of strategy *instances*
/// declared by a specific graph.
///
/// Built once per graph load:
/// 1. Host calls [`AuthStrategyRegistry::with_builtins`] to get factories.
/// 2. Loader calls [`AuthStrategyRegistry::build_from_decls`] to instantiate
///    the graph's declarations into per-name strategy instances.
/// 3. HTTP handler calls [`AuthStrategyRegistry::get`] at node execution.
pub struct AuthStrategyRegistry {
    factories: HashMap<String, AuthStrategyFactory>,
    instances: HashMap<String, (AuthStrategyDecl, Arc<dyn AuthStrategy>)>,
}

impl AuthStrategyRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
            instances: HashMap::new(),
        }
    }

    /// A registry pre-populated with psflow's four built-in strategy types.
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        strategies::register_builtins(&mut reg);
        reg
    }

    /// Register a custom strategy-type factory. Overwrites an existing
    /// registration with the same type name.
    pub fn register_factory(&mut self, type_name: impl Into<String>, factory: AuthStrategyFactory) {
        self.factories.insert(type_name.into(), factory);
    }

    /// Validate a single declaration's shape without building an instance.
    ///
    /// Checks that `type_` is registered and all of the strategy's
    /// `required_roles()` are covered by the declaration's `secrets` map.
    /// Does not resolve secrets.
    pub fn validate_decl(&self, name: &str, decl: &AuthStrategyDecl) -> Result<(), AuthError> {
        let factory =
            self.factories
                .get(&decl.type_)
                .ok_or_else(|| AuthError::UnknownStrategyType {
                    type_: decl.type_.clone(),
                })?;
        let instance = factory(decl)?;
        for role in instance.required_roles() {
            if !decl.secrets.contains_key(*role) {
                return Err(AuthError::MissingRole {
                    name: name.to_string(),
                    role: (*role).to_string(),
                });
            }
        }
        Ok(())
    }

    /// Instantiate every declared strategy and store it by name. Returns
    /// the first error encountered (shape errors surface here at graph
    /// load time).
    pub fn build_from_decls(
        &mut self,
        decls: &std::collections::BTreeMap<String, AuthStrategyDecl>,
    ) -> Result<(), AuthError> {
        for (name, decl) in decls {
            let factory =
                self.factories
                    .get(&decl.type_)
                    .ok_or_else(|| AuthError::UnknownStrategyType {
                        type_: decl.type_.clone(),
                    })?;
            let instance = factory(decl)?;
            for role in instance.required_roles() {
                if !decl.secrets.contains_key(*role) {
                    return Err(AuthError::MissingRole {
                        name: name.clone(),
                        role: (*role).to_string(),
                    });
                }
            }
            self.instances
                .insert(name.clone(), (decl.clone(), instance));
        }
        Ok(())
    }

    /// Look up a declared strategy instance by graph-local name.
    pub fn get(&self, name: &str) -> Option<(&AuthStrategyDecl, &Arc<dyn AuthStrategy>)> {
        self.instances.get(name).map(|(d, s)| (d, s))
    }

    /// True iff `name` was declared and successfully built.
    pub fn contains(&self, name: &str) -> bool {
        self.instances.contains_key(name)
    }

    pub fn declared_names(&self) -> Vec<&str> {
        self.instances.keys().map(String::as_str).collect()
    }

    pub fn registered_types(&self) -> Vec<&str> {
        self.factories.keys().map(String::as_str).collect()
    }

    /// Validate the graph's declared strategies and all `config.auth` node
    /// references without instantiating anything.
    ///
    /// - Every [`AuthStrategyDecl`] in `GraphMetadata::auth` must have a
    ///   registered type and satisfy `required_roles`.
    /// - Every node whose `config.auth` is set must reference a declared
    ///   strategy name.
    /// - Any node whose handler is [`WS_HANDLER_NAME`] and
    ///   whose `config.auth` is set must reference a strategy whose type
    ///   supports the WS handshake surface. Detected at load time so that
    ///   graph authors do not wait for a runtime handshake error.
    pub fn validate_graph(&self, graph: &crate::graph::Graph) -> Result<(), AuthError> {
        let decls = &graph.metadata().auth;
        for (name, decl) in decls {
            self.validate_decl(name, decl)?;
        }
        for node in graph.nodes() {
            let Some(auth_name) = node.config.get("auth").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(decl) = decls.get(auth_name) else {
                return Err(AuthError::UndeclaredStrategy {
                    name: auth_name.to_string(),
                });
            };
            // WS-compatibility check: only run on nodes that actually use the
            // WS handler. Other handlers resolve auth against their own
            // transport — no need to cross-check here.
            if node.handler.as_deref() == Some(WS_HANDLER_NAME) {
                // Build a transient instance to query `supports_ws()` without
                // holding instance state at load time.
                if let Some(factory) = self.factories.get(&decl.type_) {
                    let instance = factory(decl)?;
                    if !instance.supports_ws() {
                        return Err(AuthError::Config {
                            name: auth_name.to_string(),
                            message: format!(
                                "auth strategy type '{}' does not support the WebSocket handshake surface",
                                decl.type_
                            ),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

impl Default for AuthStrategyRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_registers_four_types() {
        let reg = AuthStrategyRegistry::with_builtins();
        let mut types = reg.registered_types();
        types.sort();
        assert_eq!(types, vec!["bearer", "cookie_jar", "hmac", "static_header"]);
    }

    #[test]
    fn validate_decl_unknown_type_errors() {
        let reg = AuthStrategyRegistry::with_builtins();
        let decl = AuthStrategyDecl::new("no_such_type");
        let err = reg.validate_decl("auth1", &decl).unwrap_err();
        assert!(matches!(err, AuthError::UnknownStrategyType { .. }));
    }

    #[test]
    fn validate_decl_missing_role_errors() {
        let reg = AuthStrategyRegistry::with_builtins();
        // bearer requires role "token"
        let decl = AuthStrategyDecl::new("bearer");
        let err = reg.validate_decl("b", &decl).unwrap_err();
        assert!(matches!(err, AuthError::MissingRole { role, .. } if role == "token"));
    }

    #[test]
    fn build_from_decls_populates_instances() {
        let mut reg = AuthStrategyRegistry::with_builtins();
        let mut decls = std::collections::BTreeMap::new();
        decls.insert(
            "b".into(),
            AuthStrategyDecl::new("bearer").with_secret("token", "api_key"),
        );
        reg.build_from_decls(&decls).unwrap();
        assert!(reg.contains("b"));
    }
}
