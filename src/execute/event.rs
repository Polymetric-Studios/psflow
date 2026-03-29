use crate::error::NodeError;
use crate::execute::lifecycle::NodeState;
use crate::execute::Outputs;
use std::time::{Duration, Instant};

/// Events emitted during graph execution.
#[derive(Debug, Clone)]
pub enum ExecutionEvent {
    StateChanged {
        node_id: String,
        from: NodeState,
        to: NodeState,
        timestamp: Instant,
    },
    NodeCompleted {
        node_id: String,
        outputs: Outputs,
    },
    NodeFailed {
        node_id: String,
        error: NodeError,
    },
    ExecutionStarted {
        timestamp: Instant,
    },
    ExecutionCompleted {
        elapsed: Duration,
    },
    NodeRetrying {
        node_id: String,
        attempt: u32,
        max_attempts: u32,
        error: NodeError,
        next_delay_ms: u64,
    },
}
