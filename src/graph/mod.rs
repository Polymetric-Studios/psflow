pub mod edge;
pub mod metadata;
pub mod node;
pub mod port;
pub mod types;
mod validation;

use crate::error::GraphError;
use edge::EdgeData;
use metadata::GraphMetadata;
use node::{Node, NodeId};
use petgraph::stable_graph::{EdgeIndex, NodeIndex, StableDiGraph};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Execution directive parsed from subgraph labels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubgraphDirective {
    None,
    Parallel,
    Race,
    Event,
    Loop,
    /// A named subgraph invocable as a function.
    Named(String),
}

/// A group of nodes with an optional execution directive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Subgraph {
    pub id: String,
    pub label: String,
    pub directive: SubgraphDirective,
    pub nodes: Vec<NodeId>,
    pub children: Vec<Subgraph>,
}

impl Subgraph {
    /// Recursively collect all node IDs from this subgraph and its children.
    pub fn all_node_ids(&self) -> Vec<NodeId> {
        let mut ids = self.nodes.clone();
        for child in &self.children {
            ids.extend(child.all_node_ids());
        }
        ids
    }
}

/// Result of analyzing a subgraph's boundary with the rest of the graph.
#[derive(Debug, Clone, PartialEq)]
pub struct SubgraphTopology {
    /// Nodes inside the subgraph with at least one incoming edge from outside.
    pub entry_nodes: Vec<NodeId>,
    /// Nodes inside the subgraph with at least one outgoing edge to outside.
    pub exit_nodes: Vec<NodeId>,
}

/// The core graph data structure: nodes with typed ports connected by directed edges.
///
/// Backed by `petgraph::StableDiGraph` for efficient graph operations with stable indices.
#[derive(Clone)]
pub struct Graph {
    pub(crate) inner: StableDiGraph<Node, EdgeData>,
    pub(crate) node_map: HashMap<NodeId, NodeIndex>,
    pub(crate) subgraphs: Vec<Subgraph>,
    pub(crate) metadata: GraphMetadata,
}

impl Graph {
    pub fn new() -> Self {
        Self {
            inner: StableDiGraph::new(),
            node_map: HashMap::new(),
            subgraphs: Vec::new(),
            metadata: GraphMetadata::default(),
        }
    }

    pub fn with_metadata(metadata: GraphMetadata) -> Self {
        Self {
            metadata,
            ..Self::new()
        }
    }

    // -- Node operations --

    pub fn add_node(&mut self, node: Node) -> Result<(), GraphError> {
        if self.node_map.contains_key(&node.id) {
            return Err(GraphError::DuplicateNodeId {
                id: node.id.0.clone(),
            });
        }
        let id = node.id.clone();
        let idx = self.inner.add_node(node);
        self.node_map.insert(id, idx);
        Ok(())
    }

    pub fn remove_node(&mut self, id: &NodeId) -> Result<Node, GraphError> {
        let idx = self
            .node_map
            .remove(id)
            .ok_or_else(|| GraphError::NodeNotFound { id: id.0.clone() })?;
        self.inner
            .remove_node(idx)
            .ok_or_else(|| GraphError::NodeNotFound { id: id.0.clone() })
    }

    pub fn node(&self, id: &NodeId) -> Option<&Node> {
        let idx = self.node_map.get(id)?;
        self.inner.node_weight(*idx)
    }

    pub fn node_mut(&mut self, id: &NodeId) -> Option<&mut Node> {
        let idx = self.node_map.get(id)?;
        self.inner.node_weight_mut(*idx)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &Node> {
        self.inner.node_weights()
    }

    pub fn node_count(&self) -> usize {
        self.inner.node_count()
    }

    // -- Edge operations --

    pub fn add_edge(
        &mut self,
        source: &NodeId,
        source_port: &str,
        target: &NodeId,
        target_port: &str,
        label: Option<String>,
    ) -> Result<(), GraphError> {
        let src_idx = *self
            .node_map
            .get(source)
            .ok_or_else(|| GraphError::NodeNotFound {
                id: source.0.clone(),
            })?;
        let tgt_idx = *self
            .node_map
            .get(target)
            .ok_or_else(|| GraphError::NodeNotFound {
                id: target.0.clone(),
            })?;

        let duplicate = self
            .inner
            .edges_directed(src_idx, Direction::Outgoing)
            .any(|e| {
                e.target() == tgt_idx
                    && e.weight().source_port == source_port
                    && e.weight().target_port == target_port
            });
        if duplicate {
            return Err(GraphError::DuplicateEdge {
                source_node: source.0.clone(),
                source_port: source_port.to_string(),
                target_node: target.0.clone(),
                target_port: target_port.to_string(),
            });
        }

        self.inner.add_edge(
            src_idx,
            tgt_idx,
            EdgeData {
                source_port: source_port.to_string(),
                target_port: target_port.to_string(),
                label,
            },
        );
        Ok(())
    }

    pub fn remove_edge(
        &mut self,
        source: &NodeId,
        source_port: &str,
        target: &NodeId,
        target_port: &str,
    ) -> Result<EdgeData, GraphError> {
        let edge_idx = self
            .find_edge(source, source_port, target, target_port)
            .ok_or_else(|| GraphError::EdgeNotFound {
                source_node: source.0.clone(),
                source_port: source_port.to_string(),
                target_node: target.0.clone(),
                target_port: target_port.to_string(),
            })?;
        self.inner
            .remove_edge(edge_idx)
            .ok_or_else(|| GraphError::EdgeNotFound {
                source_node: source.0.clone(),
                source_port: source_port.to_string(),
                target_node: target.0.clone(),
                target_port: target_port.to_string(),
            })
    }

    pub fn edges(&self) -> Vec<(&Node, &EdgeData, &Node)> {
        self.inner
            .edge_indices()
            .filter_map(|ei| {
                let (src_idx, tgt_idx) = self.inner.edge_endpoints(ei)?;
                Some((
                    self.inner.node_weight(src_idx)?,
                    self.inner.edge_weight(ei)?,
                    self.inner.node_weight(tgt_idx)?,
                ))
            })
            .collect()
    }

    pub fn edge_count(&self) -> usize {
        self.inner.edge_count()
    }

    // -- Graph queries --

    pub fn predecessors(&self, id: &NodeId) -> Vec<&Node> {
        let Some(&idx) = self.node_map.get(id) else {
            return Vec::new();
        };
        self.inner
            .neighbors_directed(idx, Direction::Incoming)
            .filter_map(|ni| self.inner.node_weight(ni))
            .collect()
    }

    /// Return all transitive predecessors (ancestors) of a node.
    ///
    /// Walks the graph backwards from `id`, collecting all nodes that are
    /// reachable via incoming edges. The result is the set of nodes on all
    /// paths leading to `id`. Useful for scoping traces and conversation
    /// history to a node's ancestor path.
    pub fn ancestors(&self, id: &NodeId) -> std::collections::HashSet<NodeId> {
        let mut visited = std::collections::HashSet::new();
        let mut stack = vec![id.clone()];
        while let Some(current) = stack.pop() {
            for pred in self.predecessors(&current) {
                if visited.insert(pred.id.clone()) {
                    stack.push(pred.id.clone());
                }
            }
        }
        visited
    }

    pub fn successors(&self, id: &NodeId) -> Vec<&Node> {
        let Some(&idx) = self.node_map.get(id) else {
            return Vec::new();
        };
        self.inner
            .neighbors_directed(idx, Direction::Outgoing)
            .filter_map(|ni| self.inner.node_weight(ni))
            .collect()
    }

    pub fn incoming_edges(&self, id: &NodeId) -> Vec<(&Node, &EdgeData)> {
        let Some(&idx) = self.node_map.get(id) else {
            return Vec::new();
        };
        self.inner
            .edges_directed(idx, Direction::Incoming)
            .filter_map(|e| {
                let src = self.inner.node_weight(e.source())?;
                Some((src, e.weight()))
            })
            .collect()
    }

    pub fn outgoing_edges(&self, id: &NodeId) -> Vec<(&EdgeData, &Node)> {
        let Some(&idx) = self.node_map.get(id) else {
            return Vec::new();
        };
        self.inner
            .edges_directed(idx, Direction::Outgoing)
            .filter_map(|e| {
                let tgt = self.inner.node_weight(e.target())?;
                Some((e.weight(), tgt))
            })
            .collect()
    }

    // -- Subgraphs --

    pub fn add_subgraph(&mut self, subgraph: Subgraph) {
        self.subgraphs.push(subgraph);
    }

    pub fn subgraphs(&self) -> &[Subgraph] {
        &self.subgraphs
    }

    /// Analyze a subgraph's boundary: which nodes have cross-boundary edges.
    ///
    /// Entry nodes have incoming edges from outside the subgraph.
    /// Exit nodes have outgoing edges to outside the subgraph.
    /// A single-node body will appear as both entry and exit.
    pub fn subgraph_topology(&self, sg: &Subgraph) -> SubgraphTopology {
        let all_nodes = sg.all_node_ids();
        let members: std::collections::HashSet<&NodeId> = all_nodes.iter().collect();

        let entry_nodes = all_nodes
            .iter()
            .filter(|nid| {
                self.predecessors(nid)
                    .iter()
                    .any(|pred| !members.contains(&pred.id))
            })
            .cloned()
            .collect();

        let exit_nodes = all_nodes
            .iter()
            .filter(|nid| {
                self.successors(nid)
                    .iter()
                    .any(|succ| !members.contains(&succ.id))
            })
            .cloned()
            .collect();

        SubgraphTopology {
            entry_nodes,
            exit_nodes,
        }
    }

    // -- Metadata --

    pub fn metadata(&self) -> &GraphMetadata {
        &self.metadata
    }

    pub fn metadata_mut(&mut self) -> &mut GraphMetadata {
        &mut self.metadata
    }

    // -- Internal helpers --

    fn find_edge(
        &self,
        source: &NodeId,
        source_port: &str,
        target: &NodeId,
        target_port: &str,
    ) -> Option<EdgeIndex> {
        let src_idx = self.node_map.get(source)?;
        let tgt_idx = self.node_map.get(target)?;
        self.inner
            .edges_directed(*src_idx, Direction::Outgoing)
            .find(|e| {
                e.target() == *tgt_idx
                    && e.weight().source_port == source_port
                    && e.weight().target_port == target_port
            })
            .map(|e| e.id())
    }
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Graph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Graph")
            .field("node_count", &self.inner.node_count())
            .field("edge_count", &self.inner.edge_count())
            .field("subgraphs", &self.subgraphs.len())
            .field("metadata", &self.metadata)
            .finish()
    }
}

// -- Serialization --

#[derive(Serialize, Deserialize)]
struct SerializedEdge {
    source: String,
    source_port: String,
    target: String,
    target_port: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

impl Serialize for Graph {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let nodes: Vec<&Node> = self.inner.node_weights().collect();
        let edges: Vec<SerializedEdge> = self
            .inner
            .edge_indices()
            .filter_map(|ei| {
                let (src_idx, tgt_idx) = self.inner.edge_endpoints(ei)?;
                let edge = self.inner.edge_weight(ei)?;
                let src = self.inner.node_weight(src_idx)?;
                let tgt = self.inner.node_weight(tgt_idx)?;
                Some(SerializedEdge {
                    source: src.id.0.clone(),
                    source_port: edge.source_port.clone(),
                    target: tgt.id.0.clone(),
                    target_port: edge.target_port.clone(),
                    label: edge.label.clone(),
                })
            })
            .collect();

        let mut state = serializer.serialize_struct("Graph", 4)?;
        state.serialize_field("metadata", &self.metadata)?;
        state.serialize_field("nodes", &nodes)?;
        state.serialize_field("edges", &edges)?;
        state.serialize_field("subgraphs", &self.subgraphs)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for Graph {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Repr {
            #[serde(default)]
            metadata: GraphMetadata,
            nodes: Vec<Node>,
            #[serde(default)]
            edges: Vec<SerializedEdge>,
            #[serde(default)]
            subgraphs: Vec<Subgraph>,
        }

        let repr = Repr::deserialize(deserializer)?;
        let mut graph = Graph::with_metadata(repr.metadata);
        graph.subgraphs = repr.subgraphs;

        for node in repr.nodes {
            graph.add_node(node).map_err(serde::de::Error::custom)?;
        }
        for edge in repr.edges {
            graph
                .add_edge(
                    &NodeId(edge.source),
                    &edge.source_port,
                    &NodeId(edge.target),
                    &edge.target_port,
                    edge.label,
                )
                .map_err(serde::de::Error::custom)?;
        }
        Ok(graph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::port::Port;
    use crate::graph::types::PortType;

    fn sample_graph() -> Graph {
        let mut g = Graph::new();
        g.metadata_mut().name = Some("test".into());

        let a = Node::new("A", "Fetch")
            .with_handler("fetch_rss")
            .with_output(Port::new(
                "articles",
                PortType::Vec(Box::new(PortType::Domain("Article".into()))),
            ));
        let b = Node::new("B", "Classify")
            .with_handler("llm_call")
            .with_input(Port::new(
                "articles",
                PortType::Vec(Box::new(PortType::Domain("Article".into()))),
            ))
            .with_output(Port::new(
                "classified",
                PortType::Vec(Box::new(PortType::Domain("ClassifiedArticle".into()))),
            ));
        let c = Node::new("C", "Archive")
            .with_handler("file_write")
            .with_input(Port::new(
                "classified",
                PortType::Vec(Box::new(PortType::Domain("ClassifiedArticle".into()))),
            ));

        g.add_node(a).unwrap();
        g.add_node(b).unwrap();
        g.add_node(c).unwrap();
        g.add_edge(&"A".into(), "articles", &"B".into(), "articles", None)
            .unwrap();
        g.add_edge(
            &"B".into(),
            "classified",
            &"C".into(),
            "classified",
            None,
        )
        .unwrap();
        g
    }

    #[test]
    fn add_and_query_nodes() {
        let g = sample_graph();
        assert_eq!(g.node_count(), 3);
        assert_eq!(g.node(&"A".into()).unwrap().label, "Fetch");
        assert_eq!(
            g.node(&"B".into()).unwrap().handler,
            Some("llm_call".into())
        );
    }

    #[test]
    fn duplicate_node_rejected() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "First")).unwrap();
        let err = g.add_node(Node::new("A", "Second")).unwrap_err();
        assert_eq!(err, GraphError::DuplicateNodeId { id: "A".into() });
    }

    #[test]
    fn add_and_query_edges() {
        let g = sample_graph();
        assert_eq!(g.edge_count(), 2);
        assert_eq!(g.edges().len(), 2);

        let outgoing = g.outgoing_edges(&"A".into());
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].0.target_port, "articles");

        let incoming = g.incoming_edges(&"C".into());
        assert_eq!(incoming.len(), 1);
        assert_eq!(incoming[0].0.id, NodeId::new("B"));
    }

    #[test]
    fn duplicate_edge_rejected() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        g.add_node(Node::new("B", "B")).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();
        let err = g
            .add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap_err();
        assert!(matches!(err, GraphError::DuplicateEdge { .. }));
    }

    #[test]
    fn edge_to_missing_node_rejected() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        let err = g
            .add_edge(&"A".into(), "out", &"Z".into(), "in", None)
            .unwrap_err();
        assert_eq!(err, GraphError::NodeNotFound { id: "Z".into() });
    }

    #[test]
    fn remove_node() {
        let mut g = sample_graph();
        let removed = g.remove_node(&"C".into()).unwrap();
        assert_eq!(removed.label, "Archive");
        assert_eq!(g.node_count(), 2);
        assert!(g.node(&"C".into()).is_none());
    }

    #[test]
    fn remove_edge() {
        let mut g = sample_graph();
        let removed = g
            .remove_edge(&"A".into(), "articles", &"B".into(), "articles")
            .unwrap();
        assert_eq!(removed.source_port, "articles");
        assert_eq!(g.edge_count(), 1);
    }

    #[test]
    fn predecessors_and_successors() {
        let g = sample_graph();
        let preds: Vec<&str> = g
            .predecessors(&"B".into())
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(preds, vec!["A"]);

        let succs: Vec<&str> = g
            .successors(&"B".into())
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(succs, vec!["C"]);
    }

    #[test]
    fn ancestors_linear_chain() {
        let g = sample_graph(); // A → B → C
        let ancestors_c = g.ancestors(&"C".into());
        assert!(ancestors_c.contains(&"A".into()));
        assert!(ancestors_c.contains(&"B".into()));
        assert!(!ancestors_c.contains(&"C".into())); // self not included
        assert_eq!(ancestors_c.len(), 2);
    }

    #[test]
    fn ancestors_diamond() {
        //   A
        //  / \
        // B   C
        //  \ /
        //   D
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        g.add_node(Node::new("B", "B")).unwrap();
        g.add_node(Node::new("C", "C")).unwrap();
        g.add_node(Node::new("D", "D")).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None).unwrap();
        g.add_edge(&"A".into(), "out", &"C".into(), "in", None).unwrap();
        g.add_edge(&"B".into(), "out", &"D".into(), "in", None).unwrap();
        g.add_edge(&"C".into(), "out", &"D".into(), "in", None).unwrap();

        // D's ancestors = {A, B, C}
        let ancestors_d = g.ancestors(&"D".into());
        assert_eq!(ancestors_d.len(), 3);
        assert!(ancestors_d.contains(&"A".into()));
        assert!(ancestors_d.contains(&"B".into()));
        assert!(ancestors_d.contains(&"C".into()));

        // B's ancestors = {A} only — not C
        let ancestors_b = g.ancestors(&"B".into());
        assert_eq!(ancestors_b.len(), 1);
        assert!(ancestors_b.contains(&"A".into()));
        assert!(!ancestors_b.contains(&"C".into()));
    }

    #[test]
    fn ancestors_root_node_has_none() {
        let g = sample_graph();
        let ancestors_a = g.ancestors(&"A".into());
        assert!(ancestors_a.is_empty());
    }

    #[test]
    fn serde_round_trip() {
        let g = sample_graph();
        let json = serde_json::to_string_pretty(&g).unwrap();
        let g2: Graph = serde_json::from_str(&json).unwrap();

        assert_eq!(g2.node_count(), g.node_count());
        assert_eq!(g2.edge_count(), g.edge_count());
        assert_eq!(g2.metadata().name, g.metadata().name);

        for node in g.nodes() {
            let n2 = g2.node(&node.id).expect("node missing after round-trip");
            assert_eq!(n2, node);
        }
    }

    #[test]
    fn serde_round_trip_with_subgraphs() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        g.add_node(Node::new("B", "B")).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();
        g.add_subgraph(Subgraph {
            id: "sg1".into(),
            label: "parallel: workers".into(),
            directive: SubgraphDirective::Parallel,
            nodes: vec!["A".into(), "B".into()],
            children: Vec::new(),
        });

        let json = serde_json::to_string_pretty(&g).unwrap();
        let g2: Graph = serde_json::from_str(&json).unwrap();

        assert_eq!(g2.subgraphs().len(), 1);
        assert_eq!(g2.subgraphs()[0].directive, SubgraphDirective::Parallel);
        assert_eq!(g2.subgraphs()[0].nodes.len(), 2);
    }

    #[test]
    fn node_mut_modifies_node() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "Original")).unwrap();
        g.node_mut(&"A".into()).unwrap().label = "Modified".into();
        assert_eq!(g.node(&"A".into()).unwrap().label, "Modified");
    }

    #[test]
    fn remove_node_cleans_up_edges() {
        let mut g = sample_graph();
        assert_eq!(g.edge_count(), 2);
        g.remove_node(&"B".into()).unwrap();
        assert_eq!(g.edge_count(), 0);
    }

    #[test]
    fn queries_return_empty_for_nonexistent_node() {
        let g = Graph::new();
        let id: NodeId = "missing".into();
        assert!(g.predecessors(&id).is_empty());
        assert!(g.successors(&id).is_empty());
        assert!(g.incoming_edges(&id).is_empty());
        assert!(g.outgoing_edges(&id).is_empty());
    }

    #[test]
    fn edge_labels_preserved() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        g.add_node(Node::new("B", "B")).unwrap();
        g.add_edge(
            &"A".into(),
            "out",
            &"B".into(),
            "in",
            Some("yes".into()),
        )
        .unwrap();

        let edges = g.edges();
        assert_eq!(edges[0].1.label, Some("yes".into()));
    }

    #[test]
    fn clone_preserves_graph() {
        let g = sample_graph();
        let g2 = g.clone();
        assert_eq!(g2.node_count(), g.node_count());
        assert_eq!(g2.edge_count(), g.edge_count());
        for node in g.nodes() {
            assert_eq!(g2.node(&node.id), g.node(&node.id));
        }
    }

    #[test]
    fn serde_with_config_and_exec() {
        let mut g = Graph::new();
        let mut a = Node::new("A", "Fetch").with_handler("fetch_rss");
        a.config = serde_json::json!({
            "url": "https://example.com",
            "max_items": 50
        });
        a.exec = serde_json::json!({
            "strategy": "fan_out",
            "fan_key": "items"
        });
        g.add_node(a).unwrap();

        let json = serde_json::to_string(&g).unwrap();
        let g2: Graph = serde_json::from_str(&json).unwrap();

        let node = g2.node(&"A".into()).unwrap();
        assert_eq!(node.config["url"], "https://example.com");
        assert_eq!(node.exec["strategy"], "fan_out");
    }

    #[test]
    fn serde_skips_empty_config_and_exec() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "Minimal")).unwrap();

        let json = serde_json::to_string(&g).unwrap();
        assert!(!json.contains("\"config\""), "empty config should be skipped");
        assert!(!json.contains("\"exec\""), "empty exec should be skipped");

        let g2: Graph = serde_json::from_str(&json).unwrap();
        let node = g2.node(&"A".into()).unwrap();
        assert!(node.config.is_object());
        assert!(node.exec.is_object());
    }

    #[test]
    fn subgraph_management() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        g.add_node(Node::new("B", "B")).unwrap();

        g.add_subgraph(Subgraph {
            id: "sg1".into(),
            label: "parallel: workers".into(),
            directive: SubgraphDirective::Parallel,
            nodes: vec!["A".into(), "B".into()],
            children: Vec::new(),
        });

        assert_eq!(g.subgraphs().len(), 1);
        assert_eq!(g.subgraphs()[0].directive, SubgraphDirective::Parallel);
    }

    // -- Subgraph topology tests --

    #[test]
    fn subgraph_topology_single_entry_single_exit() {
        // pred --> A --> B --> succ
        //          [sg: A, B]
        let mut g = Graph::new();
        for id in ["pred", "A", "B", "succ"] {
            g.add_node(Node::new(id, id)).unwrap();
        }
        g.add_edge(&"pred".into(), "", &"A".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"succ".into(), "", None).unwrap();

        let sg = Subgraph {
            id: "sg".into(),
            label: "parallel: work".into(),
            directive: SubgraphDirective::Parallel,
            nodes: vec!["A".into(), "B".into()],
            children: Vec::new(),
        };

        let topo = g.subgraph_topology(&sg);
        assert_eq!(topo.entry_nodes, vec![NodeId::new("A")]);
        assert_eq!(topo.exit_nodes, vec![NodeId::new("B")]);
    }

    #[test]
    fn subgraph_topology_multi_entry() {
        // pred --> A
        // pred --> B
        // A, B --> succ
        let mut g = Graph::new();
        for id in ["pred", "A", "B", "succ"] {
            g.add_node(Node::new(id, id)).unwrap();
        }
        g.add_edge(&"pred".into(), "", &"A".into(), "", None).unwrap();
        g.add_edge(&"pred".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"succ".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"succ".into(), "", None).unwrap();

        let sg = Subgraph {
            id: "sg".into(),
            label: "parallel: work".into(),
            directive: SubgraphDirective::Parallel,
            nodes: vec!["A".into(), "B".into()],
            children: Vec::new(),
        };

        let topo = g.subgraph_topology(&sg);
        assert_eq!(topo.entry_nodes.len(), 2);
        assert!(topo.entry_nodes.contains(&NodeId::new("A")));
        assert!(topo.entry_nodes.contains(&NodeId::new("B")));
        assert_eq!(topo.exit_nodes.len(), 2);
    }

    #[test]
    fn subgraph_topology_single_node_body() {
        // pred --> A --> succ
        // A is both entry and exit
        let mut g = Graph::new();
        for id in ["pred", "A", "succ"] {
            g.add_node(Node::new(id, id)).unwrap();
        }
        g.add_edge(&"pred".into(), "", &"A".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"succ".into(), "", None).unwrap();

        let sg = Subgraph {
            id: "sg".into(),
            label: "loop: process".into(),
            directive: SubgraphDirective::Loop,
            nodes: vec!["A".into()],
            children: Vec::new(),
        };

        let topo = g.subgraph_topology(&sg);
        assert_eq!(topo.entry_nodes, vec![NodeId::new("A")]);
        assert_eq!(topo.exit_nodes, vec![NodeId::new("A")]);
    }

    #[test]
    fn subgraph_topology_isolated_no_cross_boundary_edges() {
        // A --> B (both inside subgraph, no external edges)
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        g.add_node(Node::new("B", "B")).unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();

        let sg = Subgraph {
            id: "sg".into(),
            label: "parallel: isolated".into(),
            directive: SubgraphDirective::Parallel,
            nodes: vec!["A".into(), "B".into()],
            children: Vec::new(),
        };

        let topo = g.subgraph_topology(&sg);
        assert!(topo.entry_nodes.is_empty());
        assert!(topo.exit_nodes.is_empty());
    }
}
