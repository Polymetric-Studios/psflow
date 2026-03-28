use crate::graph::types::Value;
use std::collections::HashMap;

/// Scope for blackboard reads and writes.
///
/// Scoped reads fall through to global when the key is not found in the
/// requested scope. Writes always target the specified scope exactly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum BlackboardScope {
    Global,
    Subgraph(String),
    Node(String),
}

/// Scoped key-value store for cross-cutting execution state.
///
/// Provides three isolation levels: global (visible everywhere),
/// subgraph-local, and node-local. Scoped reads fall back to global
/// when the key isn't found in the requested scope.
#[derive(Debug, Clone, Default)]
pub struct Blackboard {
    global: HashMap<String, Value>,
    scoped: HashMap<String, HashMap<String, Value>>,
}

impl Blackboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read a value, falling back to global scope if not found in the requested scope.
    pub fn get(&self, key: &str, scope: &BlackboardScope) -> Option<&Value> {
        match scope {
            BlackboardScope::Global => self.global.get(key),
            BlackboardScope::Subgraph(id) | BlackboardScope::Node(id) => self
                .scoped
                .get(id)
                .and_then(|m| m.get(key))
                .or_else(|| self.global.get(key)),
        }
    }

    /// Write a value to the exact scope specified.
    pub fn set(&mut self, key: String, value: Value, scope: BlackboardScope) {
        match scope {
            BlackboardScope::Global => {
                self.global.insert(key, value);
            }
            BlackboardScope::Subgraph(id) | BlackboardScope::Node(id) => {
                self.scoped.entry(id).or_default().insert(key, value);
            }
        }
    }

    /// Remove a value from the exact scope specified.
    pub fn remove(&mut self, key: &str, scope: &BlackboardScope) -> Option<Value> {
        match scope {
            BlackboardScope::Global => self.global.remove(key),
            BlackboardScope::Subgraph(id) | BlackboardScope::Node(id) => {
                self.scoped.get_mut(id).and_then(|m| m.remove(key))
            }
        }
    }

    /// View all global entries.
    pub fn global(&self) -> &HashMap<String, Value> {
        &self.global
    }

    /// View entries for a specific scope (subgraph or node), without fallback.
    pub fn scope(&self, id: &str) -> Option<&HashMap<String, Value>> {
        self.scoped.get(id)
    }

    /// Remove an entire scope and all its entries.
    pub fn clear_scope(&mut self, id: &str) {
        self.scoped.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_get_set_remove() {
        let mut bb = Blackboard::new();
        let scope = BlackboardScope::Global;

        bb.set("key".into(), Value::I64(42), scope.clone());
        assert_eq!(bb.get("key", &scope), Some(&Value::I64(42)));

        bb.remove("key", &scope);
        assert_eq!(bb.get("key", &scope), None);
    }

    #[test]
    fn scoped_read_falls_through_to_global() {
        let mut bb = Blackboard::new();
        bb.set(
            "shared".into(),
            Value::String("global_val".into()),
            BlackboardScope::Global,
        );

        let sg_scope = BlackboardScope::Subgraph("sg1".into());
        assert_eq!(
            bb.get("shared", &sg_scope),
            Some(&Value::String("global_val".into())),
        );
    }

    #[test]
    fn scoped_write_does_not_affect_global() {
        let mut bb = Blackboard::new();
        bb.set(
            "key".into(),
            Value::String("global".into()),
            BlackboardScope::Global,
        );
        bb.set(
            "key".into(),
            Value::String("local".into()),
            BlackboardScope::Subgraph("sg1".into()),
        );

        assert_eq!(
            bb.get("key", &BlackboardScope::Global),
            Some(&Value::String("global".into())),
        );
        assert_eq!(
            bb.get("key", &BlackboardScope::Subgraph("sg1".into())),
            Some(&Value::String("local".into())),
        );
    }

    #[test]
    fn node_scope_isolation() {
        let mut bb = Blackboard::new();
        bb.set(
            "x".into(),
            Value::I64(1),
            BlackboardScope::Node("A".into()),
        );
        bb.set(
            "x".into(),
            Value::I64(2),
            BlackboardScope::Node("B".into()),
        );

        assert_eq!(
            bb.get("x", &BlackboardScope::Node("A".into())),
            Some(&Value::I64(1)),
        );
        assert_eq!(
            bb.get("x", &BlackboardScope::Node("B".into())),
            Some(&Value::I64(2)),
        );
    }

    #[test]
    fn clear_scope_removes_all_entries() {
        let mut bb = Blackboard::new();
        let scope = BlackboardScope::Subgraph("sg1".into());
        bb.set("a".into(), Value::I64(1), scope.clone());
        bb.set("b".into(), Value::I64(2), scope.clone());

        bb.clear_scope("sg1");
        assert_eq!(bb.get("a", &scope), None);
        assert_eq!(bb.get("b", &scope), None);
        assert!(bb.scope("sg1").is_none());
    }

    #[test]
    fn global_view() {
        let mut bb = Blackboard::new();
        bb.set("a".into(), Value::I64(1), BlackboardScope::Global);
        bb.set("b".into(), Value::I64(2), BlackboardScope::Global);
        assert_eq!(bb.global().len(), 2);
    }

    #[test]
    fn scope_view() {
        let mut bb = Blackboard::new();
        bb.set(
            "x".into(),
            Value::I64(10),
            BlackboardScope::Subgraph("sg1".into()),
        );
        let view = bb.scope("sg1").unwrap();
        assert_eq!(view.get("x"), Some(&Value::I64(10)));
        assert!(bb.scope("nonexistent").is_none());
    }
}
