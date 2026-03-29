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
}

impl NodeError {
    /// Whether this error is eligible for retry.
    ///
    /// Recoverable failures, timeouts, and adapter errors are retryable.
    /// Cancellations and type mismatches are not.
    pub fn is_retryable(&self) -> bool {
        match self {
            NodeError::Failed { recoverable, .. } => *recoverable,
            NodeError::Timeout { .. } => true,
            NodeError::AdapterError { .. } => true,
            NodeError::Cancelled { .. } => false,
            NodeError::TypeMismatch { .. } => false,
        }
    }
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
