use crate::graph::types::PortType;
use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

/// Errors produced during node execution at runtime.
#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq)]
pub enum NodeError {
    #[error("node failed: {message}")]
    Failed {
        source_message: Option<String>,
        message: String,
        recoverable: bool,
    },

    #[error("timed out after {elapsed_ms}ms (limit: {limit_ms}ms)")]
    Timeout { elapsed_ms: u64, limit_ms: u64 },

    #[error("cancelled: {reason}")]
    Cancelled { reason: String },

    #[error("type mismatch: expected {expected}, got {got}")]
    TypeMismatch { expected: String, got: String },

    #[error("adapter error ({adapter}): {message}")]
    AdapterError { adapter: String, message: String },

    /// The node is suspending execution and waiting for an external result.
    ///
    /// Handlers return this to signal that the node cannot complete on its own
    /// and needs an external caller to provide a result via
    /// `ExecutionContext::submit_result()`. The executor transitions the node
    /// to `NodeState::Suspended` instead of `Failed`.
    ///
    /// The `reason` field is informational and may describe what the node is
    /// waiting for (e.g., "awaiting human review", "external API callback").
    #[error("suspended: {reason}")]
    Suspended { reason: String },
}

/// Details of a port type mismatch between connected nodes.
#[derive(Debug, Clone, PartialEq)]
pub struct PortTypeMismatchInfo {
    pub source_node: String,
    pub source_port: String,
    pub target_node: String,
    pub target_port: String,
    pub source_type: PortType,
    pub target_type: PortType,
}

impl fmt::Display for PortTypeMismatchInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{} ({}) -> {}.{} ({})",
            self.source_node,
            self.source_port,
            self.source_type,
            self.target_node,
            self.target_port,
            self.target_type
        )
    }
}

/// Errors produced during graph construction or validation.
#[derive(Debug, Clone, Error, PartialEq)]
pub enum GraphError {
    #[error("cycle detected involving nodes: {nodes:?}")]
    CycleDetected { nodes: Vec<String> },

    #[error("orphan node with no connections: {node_id}")]
    OrphanNode { node_id: String },

    #[error("type mismatch: {0}")]
    PortTypeMismatch(Box<PortTypeMismatchInfo>),

    #[error("duplicate node ID: {id}")]
    DuplicateNodeId { id: String },

    #[error("duplicate edge: {source_node}.{source_port} -> {target_node}.{target_port}")]
    DuplicateEdge {
        source_node: String,
        source_port: String,
        target_node: String,
        target_port: String,
    },

    #[error("missing required input: {node_id}.{port_name}")]
    MissingRequiredInput { node_id: String, port_name: String },

    #[error("port not found: {node_id}.{port_name}")]
    PortNotFound { node_id: String, port_name: String },

    #[error("edge not found: {source_node}.{source_port} -> {target_node}.{target_port}")]
    EdgeNotFound {
        source_node: String,
        source_port: String,
        target_node: String,
        target_port: String,
    },

    #[error("node not found: {id}")]
    NodeNotFound { id: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_error_serde_round_trip() {
        let errors = vec![
            NodeError::Failed {
                source_message: Some("upstream".into()),
                message: "something broke".into(),
                recoverable: true,
            },
            NodeError::Timeout {
                elapsed_ms: 5000,
                limit_ms: 3000,
            },
            NodeError::Cancelled {
                reason: "user abort".into(),
            },
            NodeError::TypeMismatch {
                expected: "string".into(),
                got: "i64".into(),
            },
            NodeError::AdapterError {
                adapter: "mock".into(),
                message: "connection refused".into(),
            },
            NodeError::Suspended {
                reason: "awaiting human review".into(),
            },
        ];

        for err in &errors {
            let json = serde_json::to_string(err).unwrap();
            let parsed: NodeError = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, err);
        }
    }

    #[test]
    fn node_error_display_messages() {
        assert!(NodeError::Failed {
            source_message: None,
            message: "boom".into(),
            recoverable: false,
        }
        .to_string()
        .contains("boom"));

        assert!(NodeError::Timeout {
            elapsed_ms: 100,
            limit_ms: 50,
        }
        .to_string()
        .contains("100ms"));

        assert!(NodeError::Cancelled {
            reason: "test".into(),
        }
        .to_string()
        .contains("test"));
    }

    #[test]
    fn graph_error_display_messages() {
        assert!(GraphError::DuplicateNodeId { id: "A".into() }
            .to_string()
            .contains("A"));

        assert!(GraphError::NodeNotFound { id: "X".into() }
            .to_string()
            .contains("X"));

        let mismatch = PortTypeMismatchInfo {
            source_node: "A".into(),
            source_port: "out".into(),
            target_node: "B".into(),
            target_port: "in".into(),
            source_type: PortType::String,
            target_type: PortType::I64,
        };
        let display = mismatch.to_string();
        assert!(display.contains("A.out"));
        assert!(display.contains("B.in"));
    }
}
