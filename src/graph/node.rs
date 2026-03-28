use crate::graph::port::Port;
use serde::{Deserialize, Serialize};

/// Unique identifier for a node within a graph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

impl NodeId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&str> for NodeId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl From<String> for NodeId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

fn default_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn is_empty_object(v: &serde_json::Value) -> bool {
    matches!(v, serde_json::Value::Object(m) if m.is_empty())
}

/// A processing unit in the graph with typed input/output ports and configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handler: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<Port>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<Port>,
    #[serde(default = "default_object", skip_serializing_if = "is_empty_object")]
    pub config: serde_json::Value,
    #[serde(default = "default_object", skip_serializing_if = "is_empty_object")]
    pub exec: serde_json::Value,
}

impl Node {
    pub fn new(id: impl Into<NodeId>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            handler: None,
            inputs: Vec::new(),
            outputs: Vec::new(),
            config: default_object(),
            exec: default_object(),
        }
    }

    pub fn with_handler(mut self, handler: impl Into<String>) -> Self {
        self.handler = Some(handler.into());
        self
    }

    pub fn with_input(mut self, port: Port) -> Self {
        self.inputs.push(port);
        self
    }

    pub fn with_output(mut self, port: Port) -> Self {
        self.outputs.push(port);
        self
    }

    pub fn input_port(&self, name: &str) -> Option<&Port> {
        self.inputs.iter().find(|p| p.name == name)
    }

    pub fn output_port(&self, name: &str) -> Option<&Port> {
        self.outputs.iter().find(|p| p.name == name)
    }
}
