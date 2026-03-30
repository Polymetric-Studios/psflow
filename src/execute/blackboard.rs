use crate::graph::types::Value;
use std::collections::HashMap;
use std::sync::Arc;

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

/// Controls how a child blackboard inherits from its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextInheritance {
    /// Child reads fall through to the parent's global scope.
    /// Child writes stay in the child — parent is never modified.
    ReadOnly,
    /// Child gets a snapshot copy of the parent's global data at creation time.
    /// No ongoing link to the parent. Writes stay local.
    Snapshot,
    /// Child gets an empty blackboard. No parent data is visible.
    Isolated,
}

/// Scoped key-value store for cross-cutting execution state.
///
/// Provides three isolation levels: global (visible everywhere),
/// subgraph-local, and node-local. Scoped reads fall back to global
/// when the key isn't found in the requested scope.
///
/// Supports parent context inheritance: a child blackboard can read
/// from a parent's global data (read-only) while keeping its own
/// writes private. Configure via [`ContextInheritance`].
#[derive(Debug, Clone, Default)]
pub struct Blackboard {
    global: HashMap<String, Value>,
    scoped: HashMap<String, HashMap<String, Value>>,
    /// Read-only parent global data. Reads chain: local scope → own global → parent.
    parent: Option<Arc<HashMap<String, Value>>>,
}

impl Blackboard {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a child blackboard that inherits from a parent's global data.
    ///
    /// - `ReadOnly`: reads chain through to the parent; writes stay local.
    /// - `Snapshot`: copies the parent's global data; no ongoing link.
    /// - `Isolated`: empty blackboard, no parent data.
    pub fn with_parent(
        parent: &Blackboard,
        inheritance: ContextInheritance,
    ) -> Self {
        match inheritance {
            ContextInheritance::ReadOnly => Self {
                global: HashMap::new(),
                scoped: HashMap::new(),
                parent: Some(Arc::new(parent.global.clone())),
            },
            ContextInheritance::Snapshot => Self {
                global: parent.global.clone(),
                scoped: HashMap::new(),
                parent: None,
            },
            ContextInheritance::Isolated => Self::new(),
        }
    }

    /// Read a value, falling back to global scope (and then parent) if not found.
    ///
    /// Lookup order: requested scope → own global → parent global (if any).
    pub fn get(&self, key: &str, scope: &BlackboardScope) -> Option<&Value> {
        match scope {
            BlackboardScope::Global => self
                .global
                .get(key)
                .or_else(|| self.parent.as_ref().and_then(|p| p.get(key))),
            BlackboardScope::Subgraph(id) | BlackboardScope::Node(id) => self
                .scoped
                .get(id)
                .and_then(|m| m.get(key))
                .or_else(|| self.global.get(key))
                .or_else(|| self.parent.as_ref().and_then(|p| p.get(key))),
        }
    }

    /// Whether this blackboard has a parent context.
    pub fn has_parent(&self) -> bool {
        self.parent.is_some()
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

    // -- Context inheritance tests --

    #[test]
    fn read_only_inherits_parent_global() {
        let mut parent = Blackboard::new();
        parent.set("shared".into(), Value::I64(42), BlackboardScope::Global);

        let child = Blackboard::with_parent(&parent, ContextInheritance::ReadOnly);

        // Child sees parent's global data
        assert_eq!(
            child.get("shared", &BlackboardScope::Global),
            Some(&Value::I64(42))
        );
        assert!(child.has_parent());
    }

    #[test]
    fn read_only_child_writes_do_not_leak_to_parent() {
        let mut parent = Blackboard::new();
        parent.set("shared".into(), Value::I64(1), BlackboardScope::Global);

        let mut child = Blackboard::with_parent(&parent, ContextInheritance::ReadOnly);
        child.set("child_only".into(), Value::I64(99), BlackboardScope::Global);

        // Child sees its own write
        assert_eq!(
            child.get("child_only", &BlackboardScope::Global),
            Some(&Value::I64(99))
        );
        // Parent does not see child's write
        assert_eq!(parent.get("child_only", &BlackboardScope::Global), None);
    }

    #[test]
    fn read_only_child_shadows_parent() {
        let mut parent = Blackboard::new();
        parent.set("key".into(), Value::String("parent".into()), BlackboardScope::Global);

        let mut child = Blackboard::with_parent(&parent, ContextInheritance::ReadOnly);
        child.set("key".into(), Value::String("child".into()), BlackboardScope::Global);

        // Child sees its own value (shadows parent)
        assert_eq!(
            child.get("key", &BlackboardScope::Global),
            Some(&Value::String("child".into()))
        );
        // Parent is unchanged
        assert_eq!(
            parent.get("key", &BlackboardScope::Global),
            Some(&Value::String("parent".into()))
        );
    }

    #[test]
    fn read_only_scoped_reads_chain_through_parent() {
        let mut parent = Blackboard::new();
        parent.set("from_parent".into(), Value::Bool(true), BlackboardScope::Global);

        let child = Blackboard::with_parent(&parent, ContextInheritance::ReadOnly);

        // Subgraph scope falls through to child global, then to parent global
        assert_eq!(
            child.get("from_parent", &BlackboardScope::Subgraph("sg1".into())),
            Some(&Value::Bool(true))
        );
        // Node scope also chains through
        assert_eq!(
            child.get("from_parent", &BlackboardScope::Node("N1".into())),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn snapshot_copies_parent_data() {
        let mut parent = Blackboard::new();
        parent.set("data".into(), Value::I64(10), BlackboardScope::Global);

        let child = Blackboard::with_parent(&parent, ContextInheritance::Snapshot);

        // Child has a copy of parent data
        assert_eq!(
            child.get("data", &BlackboardScope::Global),
            Some(&Value::I64(10))
        );
        // No ongoing link — has_parent is false
        assert!(!child.has_parent());
    }

    #[test]
    fn snapshot_is_independent_of_parent() {
        let mut parent = Blackboard::new();
        parent.set("data".into(), Value::I64(10), BlackboardScope::Global);

        let mut child = Blackboard::with_parent(&parent, ContextInheritance::Snapshot);

        // Modify parent after snapshot — child unaffected
        parent.set("data".into(), Value::I64(999), BlackboardScope::Global);
        assert_eq!(
            child.get("data", &BlackboardScope::Global),
            Some(&Value::I64(10))
        );

        // Modify child — parent unaffected
        child.set("data".into(), Value::I64(20), BlackboardScope::Global);
        assert_eq!(
            parent.get("data", &BlackboardScope::Global),
            Some(&Value::I64(999))
        );
    }

    #[test]
    fn isolated_gets_empty_blackboard() {
        let mut parent = Blackboard::new();
        parent.set("secret".into(), Value::String("hidden".into()), BlackboardScope::Global);

        let child = Blackboard::with_parent(&parent, ContextInheritance::Isolated);

        assert_eq!(child.get("secret", &BlackboardScope::Global), None);
        assert!(!child.has_parent());
        assert!(child.global().is_empty());
    }
}
