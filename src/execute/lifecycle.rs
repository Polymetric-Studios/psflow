use crate::error::NodeError;
use serde::{Deserialize, Serialize};

/// Node execution state. Transitions enforced by `can_transition_to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeState {
    Idle,
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
    /// Node was explicitly deactivated (`exec.activate = false`).
    /// Unlike `Cancelled`, skipped nodes pass their inputs through as outputs
    /// and do not propagate cancellation to downstream nodes.
    Skipped,
    /// Node has yielded control and is waiting for an external result.
    ///
    /// A handler returns `NodeError::Suspended` to enter this state. The node
    /// stays here until `ExecutionContext::submit_result()` provides a result
    /// and transitions it to `Completed`. Unlike terminal states, `Suspended`
    /// blocks downstream nodes from executing but does not cause the graph to
    /// be considered "complete".
    ///
    /// This is the engine-level primitive for external-await patterns: the
    /// caller drives the graph via `tick()`, discovers suspended nodes, collects
    /// results externally, submits them, and resumes ticking.
    Suspended,
}

impl NodeState {
    /// Whether this state is terminal (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            NodeState::Completed | NodeState::Failed | NodeState::Cancelled | NodeState::Skipped
        )
    }

    /// Whether this state blocks downstream but is not yet truly complete.
    ///
    /// `Suspended` nodes prevent downstream execution (predecessors must be
    /// terminal for successors to fire) but the graph is not "done" while
    /// any node is suspended. Use `is_blocking()` for predecessor checks
    /// and `is_terminal()` for completion checks.
    pub fn is_suspended(&self) -> bool {
        matches!(self, NodeState::Suspended)
    }

    /// Whether this state prevents downstream nodes from executing.
    ///
    /// Both terminal states and `Suspended` satisfy predecessor checks,
    /// meaning a downstream node considers a suspended predecessor as
    /// "done enough" to not fire. Only `submit_result()` can transition
    /// a suspended node to `Completed` and unblock the graph.
    ///
    /// Note: this returns true for terminal states AND suspended. For
    /// the stepped executor's readiness check, predecessors must be
    /// terminal (not merely suspended) to allow successors to fire.
    pub fn is_done_or_suspended(&self) -> bool {
        self.is_terminal() || self.is_suspended()
    }

    /// Whether a transition from self to target is valid.
    pub fn can_transition_to(&self, target: NodeState) -> bool {
        matches!(
            (self, target),
            (NodeState::Idle, NodeState::Pending)
                | (NodeState::Idle, NodeState::Cancelled)
                | (NodeState::Idle, NodeState::Skipped)
                | (NodeState::Pending, NodeState::Running)
                | (NodeState::Pending, NodeState::Cancelled)
                | (NodeState::Running, NodeState::Completed)
                | (NodeState::Running, NodeState::Failed)
                | (NodeState::Running, NodeState::Cancelled)
                | (NodeState::Running, NodeState::Suspended)
                | (NodeState::Suspended, NodeState::Completed)
                | (NodeState::Suspended, NodeState::Failed)
                | (NodeState::Suspended, NodeState::Cancelled)
        )
    }

    /// Attempt to transition, returning the new state or an error.
    pub fn transition(self, target: NodeState) -> Result<NodeState, NodeError> {
        if self.can_transition_to(target) {
            Ok(target)
        } else {
            Err(NodeError::Failed {
                source_message: None,
                message: format!("invalid state transition: {self:?} -> {target:?}"),
                recoverable: false,
            })
        }
    }
}

impl std::fmt::Display for NodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeState::Idle => write!(f, "idle"),
            NodeState::Pending => write!(f, "pending"),
            NodeState::Running => write!(f, "running"),
            NodeState::Completed => write!(f, "completed"),
            NodeState::Failed => write!(f, "failed"),
            NodeState::Cancelled => write!(f, "cancelled"),
            NodeState::Skipped => write!(f, "skipped"),
            NodeState::Suspended => write!(f, "suspended"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_transitions() {
        assert!(NodeState::Idle.can_transition_to(NodeState::Pending));
        assert!(NodeState::Pending.can_transition_to(NodeState::Running));
        assert!(NodeState::Pending.can_transition_to(NodeState::Cancelled));
        assert!(NodeState::Running.can_transition_to(NodeState::Completed));
        assert!(NodeState::Running.can_transition_to(NodeState::Failed));
        assert!(NodeState::Running.can_transition_to(NodeState::Cancelled));
    }

    #[test]
    fn invalid_transitions() {
        assert!(!NodeState::Idle.can_transition_to(NodeState::Running));
        assert!(!NodeState::Idle.can_transition_to(NodeState::Completed));
        assert!(!NodeState::Completed.can_transition_to(NodeState::Running));
        assert!(!NodeState::Failed.can_transition_to(NodeState::Pending));
        assert!(!NodeState::Cancelled.can_transition_to(NodeState::Idle));
    }

    #[test]
    fn terminal_states() {
        assert!(!NodeState::Idle.is_terminal());
        assert!(!NodeState::Pending.is_terminal());
        assert!(!NodeState::Running.is_terminal());
        assert!(NodeState::Completed.is_terminal());
        assert!(NodeState::Failed.is_terminal());
        assert!(NodeState::Cancelled.is_terminal());
        assert!(NodeState::Skipped.is_terminal());
        // Suspended is NOT terminal — it blocks but doesn't complete.
        assert!(!NodeState::Suspended.is_terminal());
    }

    #[test]
    fn suspended_state() {
        // Running -> Suspended is valid
        assert!(NodeState::Running.can_transition_to(NodeState::Suspended));
        // Suspended -> Completed is valid (external result submitted)
        assert!(NodeState::Suspended.can_transition_to(NodeState::Completed));
        // Suspended -> Failed is valid (external failure)
        assert!(NodeState::Suspended.can_transition_to(NodeState::Failed));
        // Suspended -> Cancelled is valid
        assert!(NodeState::Suspended.can_transition_to(NodeState::Cancelled));
        // Idle -> Suspended is NOT valid (must go through Running)
        assert!(!NodeState::Idle.can_transition_to(NodeState::Suspended));
        // Suspended -> Running is NOT valid (cannot go backwards)
        assert!(!NodeState::Suspended.can_transition_to(NodeState::Running));
    }

    #[test]
    fn suspended_is_not_terminal_but_is_blocking() {
        assert!(!NodeState::Suspended.is_terminal());
        assert!(NodeState::Suspended.is_suspended());
        assert!(NodeState::Suspended.is_done_or_suspended());
        // Terminal states are also done_or_suspended
        assert!(NodeState::Completed.is_done_or_suspended());
        // Non-terminal, non-suspended are not
        assert!(!NodeState::Running.is_done_or_suspended());
    }

    #[test]
    fn suspended_display() {
        assert_eq!(NodeState::Suspended.to_string(), "suspended");
    }

    #[test]
    fn skipped_transitions() {
        assert!(NodeState::Idle.can_transition_to(NodeState::Skipped));
        assert!(!NodeState::Pending.can_transition_to(NodeState::Skipped));
        assert!(!NodeState::Running.can_transition_to(NodeState::Skipped));
        assert!(!NodeState::Completed.can_transition_to(NodeState::Skipped));
        assert!(!NodeState::Skipped.can_transition_to(NodeState::Pending));
    }

    #[test]
    fn skipped_display() {
        assert_eq!(NodeState::Skipped.to_string(), "skipped");
    }

    #[test]
    fn transition_success() {
        assert_eq!(
            NodeState::Idle.transition(NodeState::Pending).unwrap(),
            NodeState::Pending
        );
        assert_eq!(
            NodeState::Pending.transition(NodeState::Running).unwrap(),
            NodeState::Running
        );
    }

    #[test]
    fn transition_failure() {
        assert!(NodeState::Idle.transition(NodeState::Completed).is_err());
        assert!(NodeState::Completed.transition(NodeState::Running).is_err());
    }
}
