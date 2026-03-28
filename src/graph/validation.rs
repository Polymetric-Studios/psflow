use super::Graph;
use crate::error::{GraphError, PortTypeMismatchInfo};
use petgraph::algo::tarjan_scc;
use petgraph::visit::EdgeRef;
use petgraph::Direction;

impl Graph {
    /// Validate graph structure: orphan nodes, port existence, type compatibility,
    /// and missing required inputs.
    pub fn validate(&self) -> Vec<GraphError> {
        let mut errors = Vec::new();
        self.check_orphans(&mut errors);
        self.check_edges(&mut errors);
        self.check_required_inputs(&mut errors);
        errors
    }

    /// Validate as a DAG: all checks from `validate()` plus cycle detection.
    pub fn validate_as_dag(&self) -> Vec<GraphError> {
        let mut errors = self.validate();
        self.check_cycles(&mut errors);
        errors
    }

    fn check_cycles(&self, errors: &mut Vec<GraphError>) {
        // Self-loops
        for idx in self.inner.node_indices() {
            let has_self_loop = self
                .inner
                .edges_directed(idx, Direction::Outgoing)
                .any(|e| e.target() == idx);
            if has_self_loop {
                let id = self.inner[idx].id.0.clone();
                errors.push(GraphError::CycleDetected {
                    nodes: vec![id.clone(), id],
                });
            }
        }

        // Multi-node cycles via strongly connected components
        let sccs = tarjan_scc(&self.inner);
        for scc in sccs {
            if scc.len() > 1 {
                let nodes: Vec<String> =
                    scc.iter().map(|idx| self.inner[*idx].id.0.clone()).collect();
                errors.push(GraphError::CycleDetected { nodes });
            }
        }
    }

    fn check_orphans(&self, errors: &mut Vec<GraphError>) {
        if self.inner.node_count() <= 1 {
            return;
        }
        for idx in self.inner.node_indices() {
            let in_count = self
                .inner
                .edges_directed(idx, Direction::Incoming)
                .count();
            let out_count = self
                .inner
                .edges_directed(idx, Direction::Outgoing)
                .count();
            if in_count == 0 && out_count == 0 {
                errors.push(GraphError::OrphanNode {
                    node_id: self.inner[idx].id.0.clone(),
                });
            }
        }
    }

    fn check_edges(&self, errors: &mut Vec<GraphError>) {
        for ei in self.inner.edge_indices() {
            let Some((src_idx, tgt_idx)) = self.inner.edge_endpoints(ei) else {
                continue;
            };
            let edge = &self.inner[ei];
            let src_node = &self.inner[src_idx];
            let tgt_node = &self.inner[tgt_idx];

            // Check source output port exists (only if node defines outputs)
            let src_port = if src_node.outputs.is_empty() {
                None
            } else {
                match src_node.output_port(&edge.source_port) {
                    Some(p) => Some(p),
                    None => {
                        errors.push(GraphError::PortNotFound {
                            node_id: src_node.id.0.clone(),
                            port_name: edge.source_port.clone(),
                        });
                        continue;
                    }
                }
            };

            // Check target input port exists (only if node defines inputs)
            let tgt_port = if tgt_node.inputs.is_empty() {
                None
            } else {
                match tgt_node.input_port(&edge.target_port) {
                    Some(p) => Some(p),
                    None => {
                        errors.push(GraphError::PortNotFound {
                            node_id: tgt_node.id.0.clone(),
                            port_name: edge.target_port.clone(),
                        });
                        continue;
                    }
                }
            };

            // Type compatibility (only when both ports are typed)
            if let (Some(sp), Some(tp)) = (src_port, tgt_port) {
                if !sp.port_type.is_compatible_with(&tp.port_type) {
                    errors.push(GraphError::PortTypeMismatch(Box::new(
                        PortTypeMismatchInfo {
                            source_node: src_node.id.0.clone(),
                            source_port: edge.source_port.clone(),
                            target_node: tgt_node.id.0.clone(),
                            target_port: edge.target_port.clone(),
                            source_type: sp.port_type.clone(),
                            target_type: tp.port_type.clone(),
                        },
                    )));
                }
            }
        }
    }

    fn check_required_inputs(&self, errors: &mut Vec<GraphError>) {
        for idx in self.inner.node_indices() {
            let node = &self.inner[idx];
            for input_port in &node.inputs {
                let has_incoming = self
                    .inner
                    .edges_directed(idx, Direction::Incoming)
                    .any(|e| e.weight().target_port == input_port.name);
                if !has_incoming {
                    errors.push(GraphError::MissingRequiredInput {
                        node_id: node.id.0.clone(),
                        port_name: input_port.name.clone(),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::error::GraphError;
    use crate::graph::node::Node;
    use crate::graph::port::Port;
    use crate::graph::types::PortType;
    use crate::graph::Graph;

    #[test]
    fn valid_dag_passes() {
        let mut g = Graph::new();
        let a = Node::new("A", "A").with_output(Port::new("out", PortType::String));
        let b = Node::new("B", "B").with_input(Port::new("in", PortType::String));
        g.add_node(a).unwrap();
        g.add_node(b).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();

        let errors = g.validate_as_dag();
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
    }

    #[test]
    fn detects_cycle() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        g.add_node(Node::new("B", "B")).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();
        g.add_edge(&"B".into(), "out", &"A".into(), "in", None)
            .unwrap();

        let errors = g.validate_as_dag();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, GraphError::CycleDetected { .. })),
            "expected cycle error, got: {errors:?}",
        );
    }

    #[test]
    fn detects_self_loop() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        g.add_edge(&"A".into(), "out", &"A".into(), "in", None)
            .unwrap();

        let errors = g.validate_as_dag();
        assert!(errors
            .iter()
            .any(|e| matches!(e, GraphError::CycleDetected { .. })));
    }

    #[test]
    fn detects_orphan_node() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        g.add_node(Node::new("B", "B")).unwrap();
        g.add_node(Node::new("orphan", "Orphan")).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();

        let errors = g.validate();
        assert!(
            errors.iter().any(
                |e| matches!(e, GraphError::OrphanNode { node_id } if node_id == "orphan")
            ),
            "expected orphan error, got: {errors:?}",
        );
    }

    #[test]
    fn single_node_is_not_orphan() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();

        let errors = g.validate();
        assert!(
            !errors
                .iter()
                .any(|e| matches!(e, GraphError::OrphanNode { .. })),
            "single node should not be orphan"
        );
    }

    #[test]
    fn detects_type_mismatch() {
        let mut g = Graph::new();
        let a = Node::new("A", "A").with_output(Port::new("out", PortType::String));
        let b = Node::new("B", "B").with_input(Port::new("in", PortType::I64));
        g.add_node(a).unwrap();
        g.add_node(b).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();

        let errors = g.validate();
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, GraphError::PortTypeMismatch(_))),
            "expected type mismatch, got: {errors:?}",
        );
    }

    #[test]
    fn coercion_i64_to_f32_passes() {
        let mut g = Graph::new();
        let a = Node::new("A", "A").with_output(Port::new("out", PortType::I64));
        let b = Node::new("B", "B").with_input(Port::new("in", PortType::F32));
        g.add_node(a).unwrap();
        g.add_node(b).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();

        let errors = g.validate();
        assert!(
            errors.is_empty(),
            "i64->f32 coercion should be valid: {errors:?}"
        );
    }

    #[test]
    fn detects_missing_required_input() {
        let mut g = Graph::new();
        let a = Node::new("A", "A").with_output(Port::new("out", PortType::String));
        let b = Node::new("B", "B")
            .with_input(Port::new("in", PortType::String))
            .with_input(Port::new("config", PortType::Domain("Config".into())));
        g.add_node(a).unwrap();
        g.add_node(b).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();

        let errors = g.validate();
        assert!(
            errors.iter().any(|e| matches!(
                e,
                GraphError::MissingRequiredInput { node_id, port_name }
                    if node_id == "B" && port_name == "config"
            )),
            "expected missing input 'config', got: {errors:?}",
        );
    }

    #[test]
    fn detects_invalid_port_name() {
        let mut g = Graph::new();
        let a = Node::new("A", "A").with_output(Port::new("out", PortType::String));
        let b = Node::new("B", "B").with_input(Port::new("in", PortType::String));
        g.add_node(a).unwrap();
        g.add_node(b).unwrap();
        g.add_edge(&"A".into(), "wrong", &"B".into(), "in", None)
            .unwrap();

        let errors = g.validate();
        assert!(
            errors.iter().any(|e| matches!(
                e,
                GraphError::PortNotFound { node_id, port_name }
                    if node_id == "A" && port_name == "wrong"
            )),
            "expected port not found, got: {errors:?}",
        );
    }

    #[test]
    fn untyped_nodes_skip_port_validation() {
        let mut g = Graph::new();
        g.add_node(Node::new("A", "A")).unwrap();
        g.add_node(Node::new("B", "B")).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();

        let errors = g.validate();
        assert!(
            errors.is_empty(),
            "untyped nodes should skip validation: {errors:?}"
        );
    }

    #[test]
    fn multiple_errors_reported() {
        let mut g = Graph::new();
        let a = Node::new("A", "A").with_output(Port::new("out", PortType::String));
        let b = Node::new("B", "B").with_input(Port::new("in", PortType::I64));
        g.add_node(a).unwrap();
        g.add_node(b).unwrap();
        g.add_node(Node::new("orphan", "Orphan")).unwrap();
        g.add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();

        let errors = g.validate();
        assert!(
            errors.len() >= 2,
            "expected multiple errors, got: {errors:?}"
        );
    }
}
