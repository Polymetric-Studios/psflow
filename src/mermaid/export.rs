use crate::graph::node::Node;
use crate::graph::{Graph, Subgraph, SubgraphDirective};
use std::collections::HashSet;
use std::fmt::Write;

/// Export a Graph as an annotated Mermaid flowchart string.
pub fn export_mermaid(graph: &Graph) -> String {
    let mut out = String::new();
    let mut emitted = HashSet::new();

    // Direction from metadata, default TD
    let direction = graph
        .metadata()
        .direction
        .as_deref()
        .unwrap_or("TD");
    writeln!(out, "graph {direction}").unwrap();

    // Emit subgraphs with their nodes inside the blocks
    for sg in graph.subgraphs() {
        emit_subgraph(&mut out, graph, sg, &mut emitted, 1);
    }

    // Emit edges (nodes declared inline on first appearance)
    if graph.edge_count() > 0 {
        writeln!(out).unwrap();
    }
    for (src, edge, tgt) in graph.edges() {
        let src_ref = format_node_ref(src, &emitted);
        let tgt_ref = format_node_ref(tgt, &emitted);
        let label = edge
            .label
            .as_ref()
            .map(|l| format!("|{l}| "))
            .unwrap_or_default();
        writeln!(out, "    {src_ref} -->{label}{tgt_ref}").unwrap();
        emitted.insert(src.id.0.clone());
        emitted.insert(tgt.id.0.clone());
    }

    // Emit standalone nodes (not part of any edge or subgraph)
    let mut standalone: Vec<&Node> = graph
        .nodes()
        .filter(|n| !emitted.contains(&n.id.0))
        .collect();
    standalone.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    for node in standalone {
        writeln!(out, "    {}[{}]", node.id, node.label).unwrap();
    }

    // Emit annotations
    let mut all_nodes: Vec<&Node> = graph.nodes().collect();
    all_nodes.sort_by(|a, b| a.id.0.cmp(&b.id.0));

    let has_any_annotations = all_nodes.iter().any(|n| has_annotations(n))
        || has_graph_annotations(graph);
    if has_any_annotations {
        writeln!(out).unwrap();
    }

    // Graph-level metadata annotations
    emit_graph_annotations(&mut out, graph);

    // Node annotations
    for node in all_nodes {
        emit_node_annotations(&mut out, node);
    }

    out
}

fn format_node_ref(node: &Node, emitted: &HashSet<String>) -> String {
    if emitted.contains(&node.id.0) {
        node.id.0.clone()
    } else {
        format!("{}[{}]", node.id, node.label)
    }
}

fn emit_subgraph(
    out: &mut String,
    graph: &Graph,
    sg: &Subgraph,
    emitted: &mut HashSet<String>,
    indent: usize,
) {
    let pad = "    ".repeat(indent);
    let label_part = match &sg.directive {
        SubgraphDirective::None if sg.label.is_empty() => String::new(),
        _ => format!(" [\"{}\"]", sg.label),
    };
    writeln!(out, "{pad}subgraph {}{label_part}", sg.id).unwrap();

    // Emit nodes that belong to this subgraph
    for node_id in &sg.nodes {
        if let Some(node) = graph.node(node_id) {
            if !emitted.contains(&node.id.0) {
                writeln!(out, "{pad}    {}[{}]", node.id, node.label).unwrap();
                emitted.insert(node.id.0.clone());
            }
        }
    }

    // Emit child subgraphs
    for child in &sg.children {
        emit_subgraph(out, graph, child, emitted, indent + 1);
    }

    writeln!(out, "{pad}end").unwrap();
}

fn has_graph_annotations(graph: &Graph) -> bool {
    let m = graph.metadata();
    m.name.is_some() || m.version.is_some() || m.description.is_some() || m.author.is_some()
}

fn emit_graph_annotations(out: &mut String, graph: &Graph) {
    let m = graph.metadata();
    if let Some(name) = &m.name {
        writeln!(out, "    %% @graph name: \"{name}\"").unwrap();
    }
    if let Some(version) = &m.version {
        writeln!(out, "    %% @graph version: \"{version}\"").unwrap();
    }
    if let Some(description) = &m.description {
        writeln!(out, "    %% @graph description: \"{description}\"").unwrap();
    }
    if let Some(author) = &m.author {
        writeln!(out, "    %% @graph author: \"{author}\"").unwrap();
    }
}

fn has_annotations(node: &Node) -> bool {
    node.handler.is_some()
        || !node.inputs.is_empty()
        || !node.outputs.is_empty()
        || node
            .config
            .as_object()
            .is_some_and(|m| !m.is_empty())
        || node.exec.as_object().is_some_and(|m| !m.is_empty())
}

fn emit_node_annotations(out: &mut String, node: &Node) {
    if !has_annotations(node) {
        return;
    }

    let id = &node.id.0;

    if let Some(handler) = &node.handler {
        writeln!(out, "    %% @{id} handler: {handler}").unwrap();
    }

    if let Some(map) = node.exec.as_object() {
        if !map.is_empty() {
            emit_value_annotations(out, id, "exec", &node.exec);
        }
    }

    if let Some(map) = node.config.as_object() {
        if !map.is_empty() {
            emit_value_annotations(out, id, "config", &node.config);
        }
    }

    for port in &node.inputs {
        writeln!(
            out,
            "    %% @{id} inputs.{}: \"{}\"",
            port.name, port.port_type
        )
        .unwrap();
    }
    for port in &node.outputs {
        writeln!(
            out,
            "    %% @{id} outputs.{}: \"{}\"",
            port.name, port.port_type
        )
        .unwrap();
    }
}

fn emit_value_annotations(
    out: &mut String,
    node_id: &str,
    prefix: &str,
    value: &serde_json::Value,
) {
    if let serde_json::Value::Object(map) = value {
        for (key, val) in map {
            let path = format!("{prefix}.{key}");
            if val.is_object() {
                emit_value_annotations(out, node_id, &path, val);
            } else {
                writeln!(out, "    %% @{node_id} {path}: {}", format_value(val)).unwrap();
            }
        }
    }
}

fn format_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => format!("\"{s}\""),
        serde_json::Value::Null => "null".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mermaid::load_mermaid;

    #[test]
    fn round_trip_preserves_structure() {
        let input = "\
graph TD
    A[Fetch] --> B[Process]
    B --> C[Store]

    %% @A handler: fetch_data
    %% @A config.url: \"https://example.com\"
    %% @A outputs.data: \"string\"
    %% @B handler: process
    %% @B inputs.data: \"string\"
    %% @B outputs.result: \"i64\"
    %% @C handler: store
    %% @C inputs.result: \"i64\"
";
        let graph1 = load_mermaid(input).unwrap();
        let exported = export_mermaid(&graph1);
        let graph2 = load_mermaid(&exported).unwrap();

        assert_eq!(graph1.node_count(), graph2.node_count());
        assert_eq!(graph1.edge_count(), graph2.edge_count());

        for node in graph1.nodes() {
            let n2 = graph2.node(&node.id).expect("node missing after round-trip");
            assert_eq!(n2.handler, node.handler, "handler mismatch for {}", node.id);
            assert_eq!(n2.inputs, node.inputs, "inputs mismatch for {}", node.id);
            assert_eq!(n2.outputs, node.outputs, "outputs mismatch for {}", node.id);
            assert_eq!(n2.config, node.config, "config mismatch for {}", node.id);
        }
    }

    #[test]
    fn direction_preserved_through_round_trip() {
        let input = "flowchart LR\n    A --> B";
        let graph = load_mermaid(input).unwrap();
        let exported = export_mermaid(&graph);
        assert!(
            exported.starts_with("graph LR"),
            "expected LR direction, got: {}",
            exported.lines().next().unwrap_or("")
        );
    }

    #[test]
    fn graph_metadata_round_trip() {
        let input = "\
graph TD
    A --> B

    %% @graph name: \"Test\"
    %% @graph author: \"Me\"
";
        let graph1 = load_mermaid(input).unwrap();
        let exported = export_mermaid(&graph1);
        let graph2 = load_mermaid(&exported).unwrap();
        assert_eq!(graph2.metadata().name, Some("Test".into()));
        assert_eq!(graph2.metadata().author, Some("Me".into()));
    }

    #[test]
    fn subgraph_membership_preserved_in_export() {
        let input = "\
graph TD
    subgraph Workers [\"parallel: tasks\"]
        A[Worker 1]
        B[Worker 2]
    end
    A --> B
";
        let graph = load_mermaid(input).unwrap();
        let exported = export_mermaid(&graph);

        let sg_start = exported.find("subgraph").expect("missing subgraph");
        let sg_end = exported.find("end").expect("missing end");
        let sg_block = &exported[sg_start..sg_end];
        assert!(
            sg_block.contains("Worker 1"),
            "Worker 1 should be inside subgraph block"
        );
        assert!(
            sg_block.contains("Worker 2"),
            "Worker 2 should be inside subgraph block"
        );
    }

    #[test]
    fn export_produces_valid_mermaid() {
        let input = "graph TD\n    A[Start] -->|go| B[End]";
        let graph = load_mermaid(input).unwrap();
        let exported = export_mermaid(&graph);
        assert!(exported.starts_with("graph TD"));
        assert!(exported.contains("-->"));
    }
}
