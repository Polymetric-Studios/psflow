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

    // -- Loop support --

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
}
