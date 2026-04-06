use crate::graph::node::{Node, NodeId};
use crate::graph::{Graph, Subgraph, SubgraphDirective};
use crate::mermaid::annotation::apply_annotations;
use crate::mermaid::parse::{self, ParsedSubgraph};
use crate::mermaid::MermaidError;

/// Parse an annotated Mermaid file and produce a fully typed Graph.
///
/// The loader:
/// 1. Parses Mermaid topology (nodes, edges, subgraphs)
/// 2. Applies `%% @` annotations (handler, ports, config, exec)
/// 3. Resolves edge port connections by name/type matching
/// 4. Parses subgraph labels into execution directives
pub fn load_mermaid(input: &str) -> Result<Graph, Vec<MermaidError>> {
    let parsed = parse::parse(input).map_err(|e| vec![e])?;
    let mut graph = Graph::new();
    graph.metadata_mut().direction = Some(parsed.direction.as_str().to_string());
    let mut errors = Vec::new();

    // 1. Add all discovered nodes
    for pnode in parsed.nodes.values() {
        if let Err(e) = graph.add_node(Node::new(pnode.id.as_str(), pnode.label.as_str())) {
            errors.push(MermaidError::Graph(e));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    // 2. Apply annotations (sets handlers, ports, config, exec)
    if let Err(ann_errors) = apply_annotations(&mut graph, &parsed.annotations) {
        errors.extend(ann_errors);
    }

    // 3. Add edges with best-effort port resolution
    for pedge in &parsed.edges {
        let (src_port, tgt_port) = resolve_ports(&graph, &pedge.source, &pedge.target);
        if let Err(e) = graph.add_edge(
            &NodeId::new(&pedge.source),
            &src_port,
            &NodeId::new(&pedge.target),
            &tgt_port,
            pedge.label.clone(),
        ) {
            errors.push(MermaidError::Graph(e));
        }
    }

    // 4. Add subgraphs with directive parsing
    for sg in &parsed.subgraphs {
        graph.add_subgraph(convert_subgraph(sg));
    }

    if errors.is_empty() {
        Ok(graph)
    } else {
        Err(errors)
    }
}

/// Resolve which ports an edge connects based on port definitions.
///
/// Strategy (in priority order):
/// 1. Single output + single input → direct match
/// 2. Matching port names
/// 3. Compatible port types
/// 4. Single port on either side
/// 5. Empty port names (untyped connection)
fn resolve_ports(graph: &Graph, source_id: &str, target_id: &str) -> (String, String) {
    let src = graph.node(&NodeId::new(source_id));
    let tgt = graph.node(&NodeId::new(target_id));

    let src_outs = src.map(|n| &n.outputs[..]).unwrap_or(&[]);
    let tgt_ins = tgt.map(|n| &n.inputs[..]).unwrap_or(&[]);

    // No ports defined on either side
    if src_outs.is_empty() && tgt_ins.is_empty() {
        return (String::new(), String::new());
    }

    // Single output + single input
    if src_outs.len() == 1 && tgt_ins.len() == 1 {
        return (src_outs[0].name.clone(), tgt_ins[0].name.clone());
    }

    // Match by port name
    for out_p in src_outs {
        for in_p in tgt_ins {
            if out_p.name == in_p.name {
                return (out_p.name.clone(), in_p.name.clone());
            }
        }
    }

    // Match by type compatibility
    for out_p in src_outs {
        for in_p in tgt_ins {
            if out_p.port_type.is_compatible_with(&in_p.port_type) {
                return (out_p.name.clone(), in_p.name.clone());
            }
        }
    }

    // Fallback: use single port if only one side has them
    let src_port = if src_outs.len() == 1 {
        src_outs[0].name.clone()
    } else {
        String::new()
    };
    let tgt_port = if tgt_ins.len() == 1 {
        tgt_ins[0].name.clone()
    } else {
        String::new()
    };

    (src_port, tgt_port)
}

fn convert_subgraph(sg: &ParsedSubgraph) -> Subgraph {
    Subgraph {
        id: sg.id.clone(),
        label: sg.label.clone().unwrap_or_default(),
        directive: parse_directive(sg.label.as_deref()),
        nodes: sg.node_ids.iter().map(NodeId::new).collect(),
        children: sg.children.iter().map(convert_subgraph).collect(),
    }
}

fn parse_directive(label: Option<&str>) -> SubgraphDirective {
    let Some(label) = label else {
        return SubgraphDirective::None;
    };
    let label = label.trim();
    if label.starts_with("parallel:") {
        SubgraphDirective::Parallel
    } else if label.starts_with("race:") {
        SubgraphDirective::Race
    } else if label.starts_with("event:") {
        SubgraphDirective::Event
    } else if label.starts_with("loop:") {
        SubgraphDirective::Loop
    } else if let Some(name) = label.strip_prefix("subgraph:") {
        SubgraphDirective::Named(name.trim().to_string())
    } else {
        SubgraphDirective::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::PortType;

    #[test]
    fn load_simple_pipeline() {
        let input = "\
graph TD
    A[Fetch] --> B[Process]
    B --> C[Store]

    %% @A handler: fetch_data
    %% @A outputs.data: \"string\"
    %% @B handler: process
    %% @B inputs.data: \"string\"
    %% @B outputs.result: \"i64\"
    %% @C handler: store
    %% @C inputs.result: \"i64\"
";
        let graph = load_mermaid(input).unwrap();
        assert_eq!(graph.node_count(), 3);
        assert_eq!(graph.edge_count(), 2);

        let a = graph.node(&"A".into()).unwrap();
        assert_eq!(a.handler, Some("fetch_data".into()));
        assert_eq!(a.outputs.len(), 1);
        assert_eq!(a.outputs[0].port_type, PortType::String);

        let b = graph.node(&"B".into()).unwrap();
        assert_eq!(b.inputs.len(), 1);
        assert_eq!(b.outputs.len(), 1);

        // Verify port resolution: A.data -> B.data
        let a_edges = graph.outgoing_edges(&"A".into());
        assert_eq!(a_edges.len(), 1);
        assert_eq!(a_edges[0].0.source_port, "data");
        assert_eq!(a_edges[0].0.target_port, "data");
    }

    #[test]
    fn load_with_subgraph_directives() {
        let input = "\
graph TD
    subgraph Workers [\"parallel: fan out\"]
        A[Worker 1]
        B[Worker 2]
    end
";
        let graph = load_mermaid(input).unwrap();
        assert_eq!(graph.subgraphs().len(), 1);
        assert_eq!(graph.subgraphs()[0].directive, SubgraphDirective::Parallel);
    }

    #[test]
    fn load_with_config_annotations() {
        let input = "\
graph TD
    A[Fetch]

    %% @A handler: fetch_rss
    %% @A config.url: \"https://example.com\"
    %% @A config.max_items: 50
    %% @A config.nested.deep.value: true
";
        let graph = load_mermaid(input).unwrap();
        let a = graph.node(&"A".into()).unwrap();
        assert_eq!(a.config["url"], "https://example.com");
        assert_eq!(a.config["max_items"], 50);
        assert_eq!(a.config["nested"]["deep"]["value"], true);
    }

    #[test]
    fn load_with_edge_labels() {
        let input = "\
graph TD
    A{Decision} -->|yes| B[Accept]
    A -->|no| C[Reject]
";
        let graph = load_mermaid(input).unwrap();
        assert_eq!(graph.edge_count(), 2);
        let edges = graph.outgoing_edges(&"A".into());
        let labels: Vec<_> = edges.iter().map(|(e, _)| e.label.as_deref()).collect();
        assert!(labels.contains(&Some("yes")));
        assert!(labels.contains(&Some("no")));
    }

    #[test]
    fn load_graph_metadata() {
        let input = "\
graph TD
    A --> B

    %% @graph name: \"Test Pipeline\"
    %% @graph version: \"1.0\"
    %% @graph author: \"Test\"
";
        let graph = load_mermaid(input).unwrap();
        assert_eq!(graph.metadata().name, Some("Test Pipeline".into()));
        assert_eq!(graph.metadata().version, Some("1.0".into()));
    }

    #[test]
    fn load_race_directive() {
        let input = "\
graph TD
    subgraph Race [\"race: strategies\"]
        A[Fast] --> D{Pick}
        B[Slow] --> D
    end
";
        let graph = load_mermaid(input).unwrap();
        assert_eq!(graph.subgraphs()[0].directive, SubgraphDirective::Race);
    }

    #[test]
    fn load_dungeon_generator_example() {
        let input = include_str!("../../examples/dungeon_generator.mmd");
        let graph = load_mermaid(input).unwrap();
        assert!(graph.node_count() > 20, "expected many nodes");
        assert!(graph.edge_count() > 15, "expected many edges");
        assert!(graph.subgraphs().len() > 5, "expected multiple subgraphs");
        // Verify a specific annotated node loaded correctly
        let rooms = graph.node(&"ROOMS".into()).unwrap();
        assert_eq!(rooms.handler, Some("generate_rooms".into()));
        assert!(!rooms.outputs.is_empty());
    }

    #[test]
    fn load_with_multiline_annotation() {
        let input = "\
graph TD
    A[Plan] --> B[Build]

    %% @A handler: planner
    %% @A config.prompt: >>>
    %%   Plan the feature: {inputs.description}
    %%
    %%   Consider:
    %%   - Architecture implications
    %%   - Testing strategy
    %% <<<
    %% @B handler: builder
";
        let graph = load_mermaid(input).unwrap();
        let a = graph.node(&"A".into()).unwrap();
        assert_eq!(a.handler, Some("planner".into()));
        let prompt = a.config["prompt"].as_str().unwrap();
        assert!(prompt.contains("Plan the feature: {inputs.description}"));
        assert!(prompt.contains("- Architecture implications"));
        assert!(prompt.contains("- Testing strategy"));

        let b = graph.node(&"B".into()).unwrap();
        assert_eq!(b.handler, Some("builder".into()));
    }

    #[test]
    fn direction_stored_in_metadata() {
        let input = "flowchart LR\n    A --> B";
        let graph = load_mermaid(input).unwrap();
        assert_eq!(graph.metadata().direction, Some("LR".into()));
    }

    #[test]
    fn port_resolution_by_type() {
        let input = "\
graph TD
    A --> B

    %% @A outputs.result: \"i64\"
    %% @B inputs.value: \"f32\"
";
        let graph = load_mermaid(input).unwrap();
        let edges = graph.outgoing_edges(&"A".into());
        // i64 -> f32 coercion: resolved by type compatibility
        assert_eq!(edges[0].0.source_port, "result");
        assert_eq!(edges[0].0.target_port, "value");
    }
}
