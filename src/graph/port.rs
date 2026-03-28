use crate::graph::types::PortType;
use serde::{Deserialize, Serialize};

/// A named, typed connection point on a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
    pub name: String,
    pub port_type: PortType,
}

impl Port {
    pub fn new(name: impl Into<String>, port_type: PortType) -> Self {
        Self {
            name: name.into(),
            port_type,
        }
    }
}
