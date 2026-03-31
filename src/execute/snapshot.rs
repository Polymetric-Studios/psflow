//! Execution snapshots for checkpoint/resume of long-running workflows.
//!
//! An `ExecutionSnapshot` captures the full serializable state of a graph
//! execution at a point in time. It can be written to disk (JSON) and later
//! restored to resume execution from where it left off.

use crate::execute::blackboard::BlackboardSnapshot;
use crate::execute::lifecycle::NodeState;
use crate::execute::Outputs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A serializable snapshot of execution state for checkpoint/resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSnapshot {
    /// Per-node execution states.
    pub node_states: HashMap<String, NodeState>,
    /// Outputs produced by completed nodes.
    pub node_outputs: HashMap<String, Outputs>,
    /// Blackboard state (global + scoped, parent flattened).
    pub blackboard: BlackboardSnapshot,
    /// Branch decisions made so far.
    pub branch_decisions: HashMap<String, String>,
    /// Schema version for forward compatibility.
    pub version: u32,
}

impl ExecutionSnapshot {
    /// Current snapshot schema version.
    pub const CURRENT_VERSION: u32 = 1;

    /// Serialize to pretty-printed JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserialize from JSON.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Save snapshot to a file.
    pub fn save(&self, path: &std::path::Path) -> Result<(), std::io::Error> {
        let json = self
            .to_json()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, json)
    }

    /// Load snapshot from a file.
    pub fn load(path: &std::path::Path) -> Result<Self, std::io::Error> {
        let json = std::fs::read_to_string(path)?;
        Self::from_json(&json)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Node IDs that have already completed (should be skipped on resume).
    pub fn completed_nodes(&self) -> Vec<&str> {
        self.node_states
            .iter()
            .filter(|(_, s)| s.is_terminal())
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// Node IDs that were running when the snapshot was taken.
    /// These need to be re-executed on resume.
    pub fn interrupted_nodes(&self) -> Vec<&str> {
        self.node_states
            .iter()
            .filter(|(_, s)| matches!(s, NodeState::Running | NodeState::Pending))
            .map(|(id, _)| id.as_str())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::blackboard::{Blackboard, BlackboardScope};
    use crate::execute::context::ExecutionContext;
    use crate::graph::types::Value;

    fn make_snapshot_with_data() -> ExecutionSnapshot {
        let mut node_states = HashMap::new();
        node_states.insert("A".into(), NodeState::Completed);
        node_states.insert("B".into(), NodeState::Completed);
        node_states.insert("C".into(), NodeState::Running);
        node_states.insert("D".into(), NodeState::Idle);

        let mut node_outputs = HashMap::new();
        let mut a_out = Outputs::new();
        a_out.insert("result".into(), Value::String("hello".into()));
        node_outputs.insert("A".into(), a_out);

        let mut b_out = Outputs::new();
        b_out.insert("count".into(), Value::I64(42));
        node_outputs.insert("B".into(), b_out);

        let mut bb = Blackboard::new();
        bb.set("key".into(), Value::Bool(true), BlackboardScope::Global);
        bb.set(
            "local".into(),
            Value::I64(99),
            BlackboardScope::Subgraph("sg1".into()),
        );

        let mut branch_decisions = HashMap::new();
        branch_decisions.insert("BRANCH1".into(), "yes".into());

        ExecutionSnapshot {
            node_states,
            node_outputs,
            blackboard: bb.to_snapshot(),
            branch_decisions,
            version: ExecutionSnapshot::CURRENT_VERSION,
        }
    }

    #[test]
    fn json_round_trip() {
        let snapshot = make_snapshot_with_data();
        let json = snapshot.to_json().unwrap();
        let restored = ExecutionSnapshot::from_json(&json).unwrap();

        assert_eq!(restored.version, 1);
        assert_eq!(restored.node_states.len(), 4);
        assert_eq!(
            restored.node_states.get("A"),
            Some(&NodeState::Completed)
        );
        assert_eq!(
            restored.node_states.get("C"),
            Some(&NodeState::Running)
        );
        assert_eq!(
            restored.node_outputs.get("A").unwrap().get("result"),
            Some(&Value::String("hello".into()))
        );
        assert_eq!(
            restored.blackboard.global.get("key"),
            Some(&Value::Bool(true))
        );
        assert_eq!(
            restored.branch_decisions.get("BRANCH1"),
            Some(&"yes".to_string())
        );
    }

    #[test]
    fn completed_nodes() {
        let snapshot = make_snapshot_with_data();
        let mut completed = snapshot.completed_nodes();
        completed.sort();
        assert_eq!(completed, vec!["A", "B"]);
    }

    #[test]
    fn interrupted_nodes() {
        let snapshot = make_snapshot_with_data();
        let interrupted = snapshot.interrupted_nodes();
        assert_eq!(interrupted, vec!["C"]);
    }

    #[test]
    fn file_round_trip() {
        let snapshot = make_snapshot_with_data();
        let dir = std::env::temp_dir().join("psflow_snapshot_test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_snapshot.json");

        snapshot.save(&path).unwrap();
        let loaded = ExecutionSnapshot::load(&path).unwrap();

        assert_eq!(loaded.node_states.len(), 4);
        assert_eq!(loaded.node_outputs.len(), 2);
        assert_eq!(loaded.version, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_snapshot_round_trip() {
        // Create a context with state, take a snapshot, restore into new context
        let ctx = ExecutionContext::new();
        ctx.set_state("A", NodeState::Pending).unwrap();
        ctx.set_state("A", NodeState::Running).unwrap();
        ctx.set_state("A", NodeState::Completed).unwrap();

        let mut outputs = Outputs::new();
        outputs.insert("val".into(), Value::I64(100));
        ctx.store_outputs("A", outputs);

        {
            let mut bb = ctx.blackboard();
            bb.set("mode".into(), Value::String("fast".into()), BlackboardScope::Global);
        }
        ctx.set_branch_decision("BR", "yes".to_string());

        let snapshot = ctx.snapshot();
        assert_eq!(snapshot.node_states.get("A"), Some(&NodeState::Completed));
        assert_eq!(
            snapshot.node_outputs.get("A").unwrap().get("val"),
            Some(&Value::I64(100))
        );

        // Restore into a new context
        let ctx2 = ExecutionContext::from_snapshot(snapshot);
        assert_eq!(ctx2.get_state("A"), NodeState::Completed);
        assert_eq!(
            ctx2.get_outputs("A").unwrap().get("val"),
            Some(&Value::I64(100))
        );
        assert_eq!(ctx2.get_branch_decision("BR"), Some("yes".to_string()));
        {
            let bb = ctx2.blackboard();
            assert_eq!(
                bb.get("mode", &BlackboardScope::Global),
                Some(&Value::String("fast".into()))
            );
        }
    }

    #[test]
    fn resume_skips_completed_nodes() {
        let snapshot = make_snapshot_with_data();

        // Restored context should have completed nodes already marked
        let ctx = ExecutionContext::from_snapshot(snapshot);
        assert_eq!(ctx.get_state("A"), NodeState::Completed);
        assert_eq!(ctx.get_state("B"), NodeState::Completed);

        // Interrupted nodes are reset to Idle for re-execution
        assert_eq!(ctx.get_state("C"), NodeState::Idle);
        assert_eq!(ctx.get_state("D"), NodeState::Idle);
    }

    #[tokio::test]
    async fn snapshot_resume_integration() {
        use crate::execute::{sync_handler, Executor, HandlerRegistry, TopologicalExecutor};
        use crate::graph::node::Node;
        use crate::graph::Graph;

        // Build graph: A → B → C
        let mut graph = Graph::new();
        graph.add_node(Node::new("A", "First").with_handler("inc")).unwrap();
        graph.add_node(Node::new("B", "Second").with_handler("inc")).unwrap();
        graph.add_node(Node::new("C", "Third").with_handler("inc")).unwrap();
        graph.add_edge(&"A".into(), "out", &"B".into(), "in", None).unwrap();
        graph.add_edge(&"B".into(), "out", &"C".into(), "in", None).unwrap();

        let mut handlers = HandlerRegistry::new();
        handlers.insert(
            "inc".into(),
            sync_handler(|_node, _inputs| {
                let mut out = Outputs::new();
                out.insert("value".into(), Value::I64(1));
                Ok(out)
            }),
        );

        // Simulate: A and B completed, C not yet run
        let executor = TopologicalExecutor::new();
        let mut snapshot = ExecutionSnapshot {
            node_states: HashMap::new(),
            node_outputs: HashMap::new(),
            blackboard: BlackboardSnapshot {
                global: HashMap::new(),
                scoped: HashMap::new(),
            },
            branch_decisions: HashMap::new(),
            version: ExecutionSnapshot::CURRENT_VERSION,
        };
        snapshot.node_states.insert("A".into(), NodeState::Completed);
        snapshot.node_states.insert("B".into(), NodeState::Completed);
        let mut a_out = Outputs::new();
        a_out.insert("value".into(), Value::I64(1));
        snapshot.node_outputs.insert("A".into(), a_out.clone());
        snapshot.node_outputs.insert("B".into(), a_out);

        // JSON round-trip the snapshot
        let json = snapshot.to_json().unwrap();
        let restored = ExecutionSnapshot::from_json(&json).unwrap();

        // Resume: should only execute C
        let result = executor.resume(&graph, &handlers, restored).await.unwrap();

        // All three should be completed
        assert_eq!(result.node_states.get("A"), Some(&NodeState::Completed));
        assert_eq!(result.node_states.get("B"), Some(&NodeState::Completed));
        assert_eq!(result.node_states.get("C"), Some(&NodeState::Completed));

        // C should have outputs
        assert!(result.node_outputs.contains_key("C"));
    }

    #[test]
    fn empty_snapshot() {
        let snapshot = ExecutionSnapshot {
            node_states: HashMap::new(),
            node_outputs: HashMap::new(),
            blackboard: BlackboardSnapshot {
                global: HashMap::new(),
                scoped: HashMap::new(),
            },
            branch_decisions: HashMap::new(),
            version: ExecutionSnapshot::CURRENT_VERSION,
        };

        let json = snapshot.to_json().unwrap();
        let restored = ExecutionSnapshot::from_json(&json).unwrap();
        assert!(restored.node_states.is_empty());
        assert!(restored.node_outputs.is_empty());
    }
}
