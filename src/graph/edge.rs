use serde::{Deserialize, Serialize};

/// Data stored on a graph edge, connecting one node's output port to another's input port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeData {
    pub source_port: String,
    pub target_port: String,
    pub label: Option<String>,
}
