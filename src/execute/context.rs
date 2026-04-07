use crate::error::NodeError;
use crate::execute::blackboard::Blackboard;
use crate::execute::concurrency::ConcurrencyLimits;
use crate::execute::event::ExecutionEvent;
use crate::execute::lifecycle::NodeState;
use crate::execute::Outputs;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;
use tracing::trace;

/// Re-export `tokio_util::sync::CancellationToken` as the framework's
/// cooperative cancellation primitive. Provides `.cancel()`, `.is_cancelled()`,
/// and `.cancelled()` (an async future that resolves when cancelled).
pub use tokio_util::sync::CancellationToken;

/// Shared mutable state during graph execution.
///
/// Thread-safe via std::sync::Mutex (held only briefly, never across await points).
/// Recovers from poisoned mutexes to avoid cascading panics from handler failures.
pub struct ExecutionContext {
    node_states: Mutex<HashMap<String, NodeState>>,
    node_outputs: Mutex<HashMap<String, Outputs>>,
    events: Mutex<Vec<ExecutionEvent>>,
    cancel: CancellationToken,
    blackboard: Mutex<Blackboard>,
    /// Tracks branch decisions: maps branch node ID to the selected edge label.
    branch_decisions: Mutex<HashMap<String, String>>,
    concurrency: ConcurrencyLimits,
}

impl ExecutionContext {
    pub fn new() -> Self {
        Self {
            node_states: Mutex::new(HashMap::new()),
            node_outputs: Mutex::new(HashMap::new()),
            events: Mutex::new(Vec::new()),
            cancel: CancellationToken::new(),
            blackboard: Mutex::new(Blackboard::new()),
            branch_decisions: Mutex::new(HashMap::new()),
            concurrency: ConcurrencyLimits::new(),
        }
    }

    pub fn with_cancel(token: CancellationToken) -> Self {
        Self {
            cancel: token,
            ..Self::new()
        }
    }

    pub fn with_concurrency(token: CancellationToken, limits: ConcurrencyLimits) -> Self {
        Self {
            cancel: token,
            concurrency: limits,
            ..Self::new()
        }
    }

    /// Create a context whose blackboard inherits from a parent blackboard.
    pub fn with_parent_blackboard(
        token: CancellationToken,
        parent_bb: &Blackboard,
        inheritance: crate::execute::blackboard::ContextInheritance,
        limits: ConcurrencyLimits,
    ) -> Self {
        Self {
            cancel: token,
            blackboard: Mutex::new(Blackboard::with_parent(parent_bb, inheritance)),
            concurrency: limits,
            ..Self::new()
        }
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Transition a node to a new state, enforcing the lifecycle state machine.
    ///
    /// Returns `Err` if the transition is invalid (e.g., Completed → Running).
    /// Emits a `StateChanged` event on success.
    pub fn set_state(&self, node_id: &str, state: NodeState) -> Result<(), NodeError> {
        let old = {
            let mut states = self.node_states.lock().unwrap_or_else(|e| e.into_inner());
            let old = states.get(node_id).copied().unwrap_or(NodeState::Idle);
            if old == state {
                return Ok(());
            }
            // Enforce state machine transitions
            old.transition(state)?;
            states.insert(node_id.to_string(), state);
            old
        };
        trace!(node = node_id, from = %old, to = %state, "state transition");
        self.emit(ExecutionEvent::StateChanged {
            node_id: node_id.to_string(),
            from: old,
            to: state,
            timestamp: Instant::now(),
        });
        Ok(())
    }

    pub fn get_state(&self, node_id: &str) -> NodeState {
        self.node_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(node_id)
            .copied()
            .unwrap_or(NodeState::Idle)
    }

    pub fn store_outputs(&self, node_id: &str, outputs: Outputs) {
        self.node_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(node_id.to_string(), outputs);
    }

    pub fn get_outputs(&self, node_id: &str) -> Option<Outputs> {
        self.node_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(node_id)
            .cloned()
    }

    pub fn emit(&self, event: ExecutionEvent) {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
    }

    /// Build a trace from events collected so far, without consuming them.
    ///
    /// This gives handlers with context access a view of execution history
    /// up to this point — which nodes ran, in what order, with what outputs.
    pub fn live_trace(&self) -> crate::execute::trace::ExecutionTrace {
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        crate::execute::trace::ExecutionTrace::from_events(&events)
    }

    /// Build a trace scoped to the ancestors of a specific node.
    ///
    /// Only includes nodes on the paths leading to `node_id`, excluding
    /// parallel branches. This is the trace a node "should see" — its
    /// causal history, not the entire graph's execution.
    pub fn live_trace_for(
        &self,
        node_id: &str,
        graph: &crate::graph::Graph,
    ) -> crate::execute::trace::ExecutionTrace {
        self.live_trace().for_node(node_id, graph)
    }

    /// Number of events collected so far.
    pub fn event_count(&self) -> usize {
        self.events.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Return events emitted since the given index (non-consuming).
    pub fn events_since(&self, start: usize) -> Vec<ExecutionEvent> {
        let events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        events[start..].to_vec()
    }

    pub fn take_events(&self) -> Vec<ExecutionEvent> {
        std::mem::take(
            &mut *self
                .events
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    pub fn take_node_states(&self) -> HashMap<String, NodeState> {
        std::mem::take(
            &mut *self
                .node_states
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    pub fn take_node_outputs(&self) -> HashMap<String, Outputs> {
        std::mem::take(
            &mut *self
                .node_outputs
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        )
    }

    // -- Blackboard --

    /// Acquire a read/write lock on the blackboard.
    pub fn blackboard(&self) -> MutexGuard<'_, Blackboard> {
        self.blackboard.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -- Concurrency --

    pub fn concurrency(&self) -> &ConcurrencyLimits {
        &self.concurrency
    }

    // -- Branch decisions --

    /// Record a branch node's selected edge label.
    pub fn set_branch_decision(&self, node_id: &str, label: String) {
        self.branch_decisions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(node_id.to_string(), label);
    }

    /// Get the selected edge label for a branch node (if it made a decision).
    pub fn get_branch_decision(&self, node_id: &str) -> Option<String> {
        self.branch_decisions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(node_id)
            .cloned()
    }

    // -- Snapshot support --

    /// Capture a serializable snapshot of the current execution state.
    ///
    /// The snapshot includes node states, outputs, blackboard, and branch decisions.
    /// It can be serialized to JSON and later restored via `from_snapshot()`.
    pub fn snapshot(&self) -> crate::execute::snapshot::ExecutionSnapshot {
        let node_states = self
            .node_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let node_outputs = self
            .node_outputs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let bb = self
            .blackboard
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let blackboard = bb.to_snapshot();
        drop(bb);
        let branch_decisions = self
            .branch_decisions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        crate::execute::snapshot::ExecutionSnapshot {
            node_states,
            node_outputs,
            blackboard,
            branch_decisions,
            version: crate::execute::snapshot::ExecutionSnapshot::CURRENT_VERSION,
        }
    }

    /// Restore an execution context from a snapshot.
    ///
    /// Completed nodes retain their state and outputs.
    /// Interrupted nodes (Running/Pending) are reset to Idle for re-execution,
    /// and their stale outputs are cleared.
    pub fn from_snapshot(snapshot: crate::execute::snapshot::ExecutionSnapshot) -> Self {
        Self::from_snapshot_with(
            snapshot,
            CancellationToken::new(),
            ConcurrencyLimits::new(),
        )
    }

    /// Restore from a snapshot with specific cancel token and concurrency limits.
    ///
    /// Used by `TopologicalExecutor::resume()` to preserve the executor's
    /// external cancellation and concurrency configuration.
    pub fn from_snapshot_with(
        snapshot: crate::execute::snapshot::ExecutionSnapshot,
        cancel: CancellationToken,
        concurrency: ConcurrencyLimits,
    ) -> Self {
        // Collect terminal node IDs — only these keep their state and outputs
        let terminal_ids: std::collections::HashSet<String> = snapshot
            .node_states
            .iter()
            .filter(|(_, state)| state.is_terminal())
            .map(|(id, _)| id.clone())
            .collect();

        let node_states: HashMap<String, NodeState> = snapshot
            .node_states
            .into_iter()
            .filter(|(id, _)| terminal_ids.contains(id))
            .collect();

        // Only retain outputs for terminal nodes — discard stale partial outputs
        let node_outputs: HashMap<String, Outputs> = snapshot
            .node_outputs
            .into_iter()
            .filter(|(id, _)| terminal_ids.contains(id))
            .collect();

        Self {
            node_states: Mutex::new(node_states),
            node_outputs: Mutex::new(node_outputs),
            events: Mutex::new(Vec::new()),
            cancel,
            blackboard: Mutex::new(Blackboard::from_snapshot(snapshot.blackboard)),
            branch_decisions: Mutex::new(snapshot.branch_decisions),
            concurrency,
        }
    }

    // -- Subgraph support --

    /// Aggregate outputs from all nodes in a subgraph into a map keyed by node ID.
    ///
    /// Returns `{node_id: Map{output_key: value, ...}, ...}` for all completed
    /// nodes in the subgraph. Used for parallel result aggregation where
    /// `{{results.{subgraph_id}}}` resolves to this map.
    pub fn aggregate_subgraph_outputs(
        &self,
        node_ids: &[crate::graph::node::NodeId],
    ) -> Outputs {
        use crate::graph::types::Value;
        use std::collections::BTreeMap;

        let outputs = self.node_outputs.lock().unwrap_or_else(|e| e.into_inner());
        let mut result = Outputs::new();
        for nid in node_ids {
            if let Some(node_outputs) = outputs.get(&nid.0) {
                let map: BTreeMap<String, Value> = node_outputs
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                result.insert(nid.0.clone(), Value::Map(map));
            }
        }
        result
    }

    /// Reset node states and outputs to allow re-execution in loop iterations.
    pub fn reset_states<'a>(&self, node_ids: impl Iterator<Item = &'a str>) {
        let mut states = self.node_states.lock().unwrap_or_else(|e| e.into_inner());
        let mut outputs = self.node_outputs.lock().unwrap_or_else(|e| e.into_inner());
        for id in node_ids {
            states.remove(id);
            outputs.remove(id);
        }
    }
}

impl Default for ExecutionContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_state_enforces_transitions() {
        let ctx = ExecutionContext::new();
        // Valid: Idle → Pending
        assert!(ctx.set_state("A", NodeState::Pending).is_ok());
        // Invalid: Pending → Completed (must go through Running)
        assert!(ctx.set_state("A", NodeState::Completed).is_err());
        // Valid: Pending → Running
        assert!(ctx.set_state("A", NodeState::Running).is_ok());
        // Valid: Running → Completed
        assert!(ctx.set_state("A", NodeState::Completed).is_ok());
        // Invalid: Completed → Running (terminal)
        assert!(ctx.set_state("A", NodeState::Running).is_err());
    }

    #[test]
    fn set_state_noop_for_same_state() {
        let ctx = ExecutionContext::new();
        ctx.set_state("A", NodeState::Pending).unwrap();
        // Same state is a no-op, not an error
        assert!(ctx.set_state("A", NodeState::Pending).is_ok());
    }

    #[test]
    fn idle_to_cancelled_valid_for_cascade() {
        let ctx = ExecutionContext::new();
        assert!(ctx.set_state("A", NodeState::Cancelled).is_ok());
        assert_eq!(ctx.get_state("A"), NodeState::Cancelled);
    }

    #[test]
    fn store_and_retrieve_outputs() {
        let ctx = ExecutionContext::new();
        let mut outputs = Outputs::new();
        outputs.insert("key".into(), crate::graph::types::Value::I64(42));

        ctx.store_outputs("A", outputs.clone());
        let retrieved = ctx.get_outputs("A").unwrap();
        assert_eq!(retrieved, outputs);
    }

    #[test]
    fn get_outputs_missing_returns_none() {
        let ctx = ExecutionContext::new();
        assert!(ctx.get_outputs("missing").is_none());
    }

    #[test]
    fn branch_decision_set_and_get() {
        let ctx = ExecutionContext::new();
        ctx.set_branch_decision("B", "yes".to_string());

        assert_eq!(ctx.get_branch_decision("B"), Some("yes".to_string()));
        assert_eq!(ctx.get_branch_decision("missing"), None);
    }

    #[test]
    fn branch_decision_overwrite() {
        let ctx = ExecutionContext::new();
        ctx.set_branch_decision("B", "yes".to_string());
        ctx.set_branch_decision("B", "no".to_string());

        assert_eq!(ctx.get_branch_decision("B"), Some("no".to_string()));
    }

    #[test]
    fn blackboard_via_context() {
        let ctx = ExecutionContext::new();
        {
            let mut bb = ctx.blackboard();
            bb.set(
                "key".into(),
                crate::graph::types::Value::String("value".into()),
                crate::execute::blackboard::BlackboardScope::Global,
            );
        }
        {
            let bb = ctx.blackboard();
            assert_eq!(
                bb.get("key", &crate::execute::blackboard::BlackboardScope::Global),
                Some(&crate::graph::types::Value::String("value".into()))
            );
        }
    }

    #[test]
    fn reset_states_clears_state_and_outputs() {
        let ctx = ExecutionContext::new();
        ctx.set_state("A", NodeState::Pending).unwrap();
        ctx.set_state("A", NodeState::Running).unwrap();
        ctx.set_state("A", NodeState::Completed).unwrap();
        ctx.store_outputs("A", Outputs::new());

        ctx.reset_states(["A"].iter().copied());

        assert_eq!(ctx.get_state("A"), NodeState::Idle);
        assert!(ctx.get_outputs("A").is_none());
    }

    #[test]
    fn emit_and_take_events() {
        let ctx = ExecutionContext::new();
        ctx.emit(ExecutionEvent::ExecutionStarted {
            timestamp: std::time::Instant::now(),
        });

        let events = ctx.take_events();
        assert_eq!(events.len(), 1);

        // After take, events are empty
        let events2 = ctx.take_events();
        assert!(events2.is_empty());
    }

    #[test]
    fn live_trace_shows_history_so_far() {
        let ctx = ExecutionContext::new();

        // Simulate node A completing
        ctx.set_state("A", NodeState::Pending).unwrap();
        ctx.set_state("A", NodeState::Running).unwrap();
        ctx.store_outputs("A", {
            let mut o = Outputs::new();
            o.insert("val".into(), crate::graph::types::Value::I64(1));
            o
        });
        ctx.emit(ExecutionEvent::NodeCompleted {
            node_id: "A".into(),
            outputs: {
                let mut o = Outputs::new();
                o.insert("val".into(), crate::graph::types::Value::I64(1));
                o
            },
        });
        ctx.set_state("A", NodeState::Completed).unwrap();

        // Mid-execution: node B is running
        ctx.set_state("B", NodeState::Pending).unwrap();
        ctx.set_state("B", NodeState::Running).unwrap();

        // Live trace should show A completed, B not yet in trace (still running)
        let trace = ctx.live_trace();
        assert_eq!(trace.completed_nodes().len(), 1);
        assert_eq!(trace.completed_nodes()[0].node_id, "A");
        assert!(trace.node("A").unwrap().outputs.is_some());

        // Events are NOT consumed — take_events still works after
        let events = ctx.take_events();
        assert!(!events.is_empty());
    }

    #[test]
    fn cancellation_propagates() {
        let token = CancellationToken::new();
        let ctx = ExecutionContext::with_cancel(token.clone());

        assert!(!ctx.is_cancelled());
        token.cancel();
        assert!(ctx.is_cancelled());
    }

    #[test]
    fn event_count_tracks_emitted_events() {
        let ctx = ExecutionContext::new();
        assert_eq!(ctx.event_count(), 0);

        ctx.emit(ExecutionEvent::StateChanged {
            node_id: "A".into(),
            from: NodeState::Idle,
            to: NodeState::Pending,
            timestamp: Instant::now(),
        });
        ctx.emit(ExecutionEvent::StateChanged {
            node_id: "A".into(),
            from: NodeState::Pending,
            to: NodeState::Running,
            timestamp: Instant::now(),
        });
        ctx.emit(ExecutionEvent::StateChanged {
            node_id: "A".into(),
            from: NodeState::Running,
            to: NodeState::Completed,
            timestamp: Instant::now(),
        });

        assert_eq!(ctx.event_count(), 3);
    }

    #[test]
    fn events_since_returns_slice() {
        let ctx = ExecutionContext::new();

        let node_ids = ["A", "B", "C", "D", "E"];
        for id in &node_ids {
            ctx.emit(ExecutionEvent::StateChanged {
                node_id: (*id).into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: Instant::now(),
            });
        }

        let since_3 = ctx.events_since(3);
        assert_eq!(since_3.len(), 2);

        // Verify the returned events have the correct node IDs
        match &since_3[0] {
            ExecutionEvent::StateChanged { node_id, .. } => assert_eq!(node_id, "D"),
            other => panic!("expected StateChanged, got {other:?}"),
        }
        match &since_3[1] {
            ExecutionEvent::StateChanged { node_id, .. } => assert_eq!(node_id, "E"),
            other => panic!("expected StateChanged, got {other:?}"),
        }
    }

    #[test]
    fn events_since_empty_when_at_end() {
        let ctx = ExecutionContext::new();

        ctx.emit(ExecutionEvent::StateChanged {
            node_id: "A".into(),
            from: NodeState::Idle,
            to: NodeState::Pending,
            timestamp: Instant::now(),
        });
        ctx.emit(ExecutionEvent::StateChanged {
            node_id: "B".into(),
            from: NodeState::Idle,
            to: NodeState::Pending,
            timestamp: Instant::now(),
        });

        let count = ctx.event_count();
        let since_end = ctx.events_since(count);
        assert!(since_end.is_empty());
    }

    #[test]
    fn events_since_zero_returns_all() {
        let ctx = ExecutionContext::new();

        ctx.emit(ExecutionEvent::StateChanged {
            node_id: "A".into(),
            from: NodeState::Idle,
            to: NodeState::Pending,
            timestamp: Instant::now(),
        });
        ctx.emit(ExecutionEvent::StateChanged {
            node_id: "B".into(),
            from: NodeState::Pending,
            to: NodeState::Running,
            timestamp: Instant::now(),
        });
        ctx.emit(ExecutionEvent::StateChanged {
            node_id: "C".into(),
            from: NodeState::Running,
            to: NodeState::Completed,
            timestamp: Instant::now(),
        });

        let all = ctx.events_since(0);
        assert_eq!(all.len(), 3);

        // Verify ordering matches emission order
        let ids: Vec<&str> = all
            .iter()
            .map(|e| match e {
                ExecutionEvent::StateChanged { node_id, .. } => node_id.as_str(),
                other => panic!("expected StateChanged, got {other:?}"),
            })
            .collect();
        assert_eq!(ids, vec!["A", "B", "C"]);
    }

    #[test]
    fn aggregate_subgraph_outputs_collects_by_node_id() {
        use crate::graph::node::NodeId;
        use crate::graph::types::Value;

        let ctx = ExecutionContext::new();

        let mut out_a = Outputs::new();
        out_a.insert("result".into(), Value::String("from_a".into()));
        ctx.store_outputs("A", out_a);

        let mut out_b = Outputs::new();
        out_b.insert("result".into(), Value::String("from_b".into()));
        ctx.store_outputs("B", out_b);

        // Node C has no outputs (not completed yet)

        let node_ids = vec![NodeId::new("A"), NodeId::new("B"), NodeId::new("C")];
        let agg = ctx.aggregate_subgraph_outputs(&node_ids);

        assert_eq!(agg.len(), 2); // Only A and B
        match &agg["A"] {
            Value::Map(m) => assert_eq!(m["result"], Value::String("from_a".into())),
            other => panic!("expected Map, got {other:?}"),
        }
        match &agg["B"] {
            Value::Map(m) => assert_eq!(m["result"], Value::String("from_b".into())),
            other => panic!("expected Map, got {other:?}"),
        }
        assert!(agg.get("C").is_none());
    }

    #[test]
    fn aggregate_subgraph_outputs_multi_port_node() {
        use crate::graph::node::NodeId;
        use crate::graph::types::Value;

        let ctx = ExecutionContext::new();

        let mut out_a = Outputs::new();
        out_a.insert("text".into(), Value::String("hello".into()));
        out_a.insert("score".into(), Value::I64(42));
        out_a.insert("valid".into(), Value::Bool(true));
        ctx.store_outputs("A", out_a);

        let node_ids = vec![NodeId::new("A")];
        let agg = ctx.aggregate_subgraph_outputs(&node_ids);

        match &agg["A"] {
            Value::Map(m) => {
                assert_eq!(m["text"], Value::String("hello".into()));
                assert_eq!(m["score"], Value::I64(42));
                assert_eq!(m["valid"], Value::Bool(true));
                assert_eq!(m.len(), 3);
            }
            other => panic!("expected Map, got {other:?}"),
        }
    }
}
