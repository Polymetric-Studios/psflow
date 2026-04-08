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
}

impl NodeState {
    /// Whether this state is terminal (no further transitions).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            NodeState::Completed | NodeState::Failed | NodeState::Cancelled | NodeState::Skipped
        )
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
