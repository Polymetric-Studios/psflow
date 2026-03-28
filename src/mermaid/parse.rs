use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    TD,
    TB,
    BT,
    LR,
    RL,
}

impl Direction {
    pub fn as_str(&self) -> &str {
        match self {
            Direction::TD => "TD",
            Direction::TB => "TB",
            Direction::BT => "BT",
            Direction::LR => "LR",
            Direction::RL => "RL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NodeShape {
    #[default]
    Rectangle,
    Rounded,
    Stadium,
    Diamond,
    Hexagon,
    Circle,
    Subroutine,
    Cylindrical,
    Asymmetric,
}

#[derive(Debug, Clone)]
pub struct ParsedNode {
    pub id: String,
    pub label: String,
    pub shape: NodeShape,
}

#[derive(Debug, Clone)]
pub struct ParsedEdge {
    pub source: String,
    pub target: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ParsedSubgraph {
    pub id: String,
    pub label: Option<String>,
    pub node_ids: Vec<String>,
    pub children: Vec<ParsedSubgraph>,
}

#[derive(Debug, Clone)]
pub struct ParsedAnnotation {
    pub target_id: String,
    pub key: String,
    pub raw_value: String,
}

#[derive(Debug, Default)]
pub struct ParsedMermaid {
    pub direction: Direction,
    pub nodes: HashMap<String, ParsedNode>,
    pub edges: Vec<ParsedEdge>,
    pub subgraphs: Vec<ParsedSubgraph>,
    pub annotations: Vec<ParsedAnnotation>,
}

/// Parse Mermaid flowchart text into an intermediate representation.
pub fn parse(input: &str) -> Result<ParsedMermaid, super::MermaidError> {
    let mut result = ParsedMermaid::default();
    let mut sg_stack: Vec<ParsedSubgraph> = Vec::new();

    for (idx, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let line_num = idx + 1;

        // Graph/flowchart direction
        if let Some(dir_str) = line
            .strip_prefix("graph ")
            .or_else(|| line.strip_prefix("flowchart "))
        {
            result.direction =
                parse_direction(dir_str.trim()).ok_or_else(|| super::MermaidError::Parse {
                    line: line_num,
                    message: format!("unknown direction: {dir_str}"),
                })?;
            continue;
        }

        // Subgraph start
        if let Some(rest) = line.strip_prefix("subgraph ") {
            let (id, label) = parse_subgraph_header(rest.trim());
            sg_stack.push(ParsedSubgraph {
                id,
                label,
                node_ids: Vec::new(),
                children: Vec::new(),
            });
            continue;
        }

        // Subgraph end
        if line == "end" {
            if let Some(sg) = sg_stack.pop() {
                if let Some(parent) = sg_stack.last_mut() {
                    parent.children.push(sg);
                } else {
                    result.subgraphs.push(sg);
                }
            }
            continue;
        }

        // Annotation: %% @NodeID key: value
        if let Some(rest) = line.strip_prefix("%% @") {
            if let Some(ann) = parse_annotation(rest) {
                result.annotations.push(ann);
            }
            continue;
        }

        // Comment
        if line.starts_with("%%") {
            continue;
        }

        // Try edge declaration (supports chains: A --> B --> C)
        if let Some((nodes, edges)) = parse_edge_line(line) {
            for node in nodes {
                register_node(&mut result.nodes, &mut sg_stack, node);
            }
            result.edges.extend(edges);
            continue;
        }

        // Try standalone node declaration: A[Label]
        if let Some(node) = parse_node_ref(line) {
            register_node(&mut result.nodes, &mut sg_stack, node);
        }
    }

    // Auto-close unclosed subgraphs
    while let Some(sg) = sg_stack.pop() {
        if let Some(parent) = sg_stack.last_mut() {
            parent.children.push(sg);
        } else {
            result.subgraphs.push(sg);
        }
    }

    Ok(result)
}

fn register_node(
    nodes: &mut HashMap<String, ParsedNode>,
    sg_stack: &mut [ParsedSubgraph],
    node: ParsedNode,
) {
    let id = node.id.clone();
    let has_explicit_label = node.label != node.id;

    let should_insert = match nodes.get(&id) {
        None => true,
        // Only override if new definition has a label and existing doesn't
        Some(existing) => has_explicit_label && existing.label == existing.id,
    };

    if should_insert {
        nodes.insert(id.clone(), node);
    }

    if let Some(sg) = sg_stack.last_mut() {
        if !sg.node_ids.contains(&id) {
            sg.node_ids.push(id);
        }
    }
}

fn parse_direction(s: &str) -> Option<Direction> {
    match s {
        "TD" => Some(Direction::TD),
        "TB" => Some(Direction::TB),
        "BT" => Some(Direction::BT),
        "LR" => Some(Direction::LR),
        "RL" => Some(Direction::RL),
        _ => None,
    }
}

fn parse_subgraph_header(s: &str) -> (String, Option<String>) {
    if let Some(bracket_pos) = s.find('[') {
        let id = s[..bracket_pos].trim().to_string();
        let rest = &s[bracket_pos + 1..];
        let label = rest
            .strip_suffix(']')
            .map(|l| l.trim().trim_matches('"').to_string());
        (id, label)
    } else {
        (s.trim().to_string(), None)
    }
}

fn parse_annotation(s: &str) -> Option<ParsedAnnotation> {
    let s = s.trim();
    let space_pos = s.find(' ')?;
    let target_id = s[..space_pos].to_string();
    let rest = s[space_pos + 1..].trim();
    let colon_pos = rest.find(':')?;
    let key = rest[..colon_pos].trim().to_string();
    let raw_value = rest[colon_pos + 1..].trim().to_string();
    Some(ParsedAnnotation {
        target_id,
        key,
        raw_value,
    })
}

/// Parse an edge line, supporting chains like `A --> B --> C`.
fn parse_edge_line(line: &str) -> Option<(Vec<ParsedNode>, Vec<ParsedEdge>)> {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();

    // Find first edge operator
    let (pos, op) = find_edge_operator(line)?;
    let left = line[..pos].trim();
    let source = parse_node_ref(left)?;
    nodes.push(source.clone());

    let mut prev = source;
    let mut remaining = line[pos + op.len()..].trim();

    loop {
        // Parse optional label: |text|
        let (label, after_label) = parse_optional_label(remaining);
        remaining = after_label;

        // Check for another operator (chain)
        if let Some((next_pos, next_op)) = find_edge_operator(remaining) {
            let mid = remaining[..next_pos].trim();
            let node = parse_node_ref(mid)?;
            edges.push(ParsedEdge {
                source: prev.id.clone(),
                target: node.id.clone(),
                label,
            });
            nodes.push(node.clone());
            prev = node;
            remaining = remaining[next_pos + next_op.len()..].trim();
        } else {
            // Final node in the chain
            let node = parse_node_ref(remaining)?;
            edges.push(ParsedEdge {
                source: prev.id.clone(),
                target: node.id.clone(),
                label,
            });
            nodes.push(node);
            break;
        }
    }

    Some((nodes, edges))
}

fn find_edge_operator(s: &str) -> Option<(usize, &'static str)> {
    let operators: &[&str] = &["-.->", "==>", "-->", "---"];
    let mut best: Option<(usize, &str)> = None;
    for &op in operators {
        if let Some(pos) = s.find(op) {
            if best.is_none() || pos < best.unwrap().0 {
                best = Some((pos, op));
            }
        }
    }
    best
}

fn parse_optional_label(s: &str) -> (Option<String>, &str) {
    if let Some(stripped) = s.strip_prefix('|') {
        if let Some(end) = stripped.find('|') {
            let label = stripped[..end].to_string();
            let rest = stripped[end + 1..].trim();
            return (Some(label), rest);
        }
    }
    (None, s)
}

fn parse_node_ref(s: &str) -> Option<ParsedNode> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // ID ends at first shape delimiter or whitespace
    let id_end = s
        .find(['[', '(', '{', '>', ' ', '\t'])
        .unwrap_or(s.len());
    let id = &s[..id_end];
    if id.is_empty() {
        return None;
    }

    // Bare ID (no shape, or hit whitespace)
    if id_end >= s.len() || matches!(s.as_bytes()[id_end], b' ' | b'\t') {
        return Some(ParsedNode {
            id: id.to_string(),
            label: id.to_string(),
            shape: NodeShape::Rectangle,
        });
    }

    let rest = &s[id_end..];
    let (label, shape) = parse_shape_and_label(rest)?;

    Some(ParsedNode {
        id: id.to_string(),
        label,
        shape,
    })
}

fn parse_shape_and_label(s: &str) -> Option<(String, NodeShape)> {
    // Multi-char delimiters first (order matters for prefix overlap)
    if s.starts_with("((") {
        return Some((extract_between(s, "((", "))")?, NodeShape::Circle));
    }
    if s.starts_with("[[") {
        return Some((extract_between(s, "[[", "]]")?, NodeShape::Subroutine));
    }
    if s.starts_with("[(") {
        return Some((extract_between(s, "[(", ")]")?, NodeShape::Cylindrical));
    }
    if s.starts_with("([") {
        return Some((extract_between(s, "([", "])")?, NodeShape::Stadium));
    }
    if s.starts_with("{{") {
        return Some((extract_between(s, "{{", "}}")?, NodeShape::Hexagon));
    }
    // Single-char delimiters
    if s.starts_with('{') {
        return Some((extract_between(s, "{", "}")?, NodeShape::Diamond));
    }
    if s.starts_with('(') {
        return Some((extract_between(s, "(", ")")?, NodeShape::Rounded));
    }
    if s.starts_with('[') {
        return Some((extract_between(s, "[", "]")?, NodeShape::Rectangle));
    }
    // Asymmetric: >text]
    if s.starts_with('>') {
        let end = s.find(']')?;
        return Some((s[1..end].to_string(), NodeShape::Asymmetric));
    }
    None
}

/// Extract text between open and close delimiters, finding the first matching close.
fn extract_between(s: &str, open: &str, close: &str) -> Option<String> {
    let rest = s.strip_prefix(open)?;
    let end = rest.find(close)?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_graph() {
        let input = "graph TD\n    A[Fetch] --> B[Process]\n    B --> C[Store]";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.direction, Direction::TD);
        assert_eq!(parsed.nodes.len(), 3);
        assert_eq!(parsed.edges.len(), 2);
        assert_eq!(parsed.nodes["A"].label, "Fetch");
        assert_eq!(parsed.nodes["C"].label, "Store");
    }

    #[test]
    fn parse_node_shapes() {
        let input = "graph TD\n    A[Rect]\n    B(Round)\n    C{Diamond}\n    D((Circle))\n    E([Stadium])\n    F[[Subroutine]]";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.nodes["A"].shape, NodeShape::Rectangle);
        assert_eq!(parsed.nodes["B"].shape, NodeShape::Rounded);
        assert_eq!(parsed.nodes["C"].shape, NodeShape::Diamond);
        assert_eq!(parsed.nodes["D"].shape, NodeShape::Circle);
        assert_eq!(parsed.nodes["E"].shape, NodeShape::Stadium);
        assert_eq!(parsed.nodes["F"].shape, NodeShape::Subroutine);
    }

    #[test]
    fn parse_edge_with_label() {
        let input = "graph TD\n    A{Decision} -->|yes| B[Accept]\n    A -->|no| C[Reject]";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.edges.len(), 2);
        assert_eq!(parsed.edges[0].label, Some("yes".into()));
        assert_eq!(parsed.edges[1].label, Some("no".into()));
    }

    #[test]
    fn parse_subgraphs() {
        let input = "graph TD\n    subgraph Init [\"parallel: setup\"]\n        A[First] --> B[Second]\n    end\n    subgraph Process\n        C[Third]\n    end";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.subgraphs.len(), 2);
        assert_eq!(parsed.subgraphs[0].id, "Init");
        assert_eq!(parsed.subgraphs[0].label, Some("parallel: setup".into()));
        assert_eq!(parsed.subgraphs[0].node_ids.len(), 2);
        assert_eq!(parsed.subgraphs[1].id, "Process");
    }

    #[test]
    fn parse_annotations() {
        let input = "graph TD\n    A --> B\n\n    %% @A handler: fetch\n    %% @A config.url: \"https://x.com\"\n    %% @A outputs.data: \"string\"";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.annotations.len(), 3);
        assert_eq!(parsed.annotations[0].target_id, "A");
        assert_eq!(parsed.annotations[0].key, "handler");
        assert_eq!(parsed.annotations[0].raw_value, "fetch");
        assert_eq!(parsed.annotations[2].key, "outputs.data");
    }

    #[test]
    fn parse_bare_node_references() {
        let input = "graph TD\n    A[Source] --> B\n    A --> C[Target]";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.nodes.len(), 3);
        assert_eq!(parsed.nodes["B"].label, "B");
        assert_eq!(parsed.nodes["C"].label, "Target");
    }

    #[test]
    fn parse_flowchart_keyword() {
        let input = "flowchart LR\n    A --> B";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.direction, Direction::LR);
    }

    #[test]
    fn node_label_wins_over_bare_ref() {
        let input = "graph TD\n    A --> B\n    B[Labeled] --> C";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.nodes["B"].label, "Labeled");
    }

    #[test]
    fn nested_subgraphs() {
        let input = "graph TD\n    subgraph Outer\n        subgraph Inner\n            A --> B\n        end\n    end";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.subgraphs.len(), 1);
        assert_eq!(parsed.subgraphs[0].id, "Outer");
        assert_eq!(parsed.subgraphs[0].children.len(), 1);
        assert_eq!(parsed.subgraphs[0].children[0].id, "Inner");
    }

    #[test]
    fn parse_chained_edges() {
        let input = "graph TD\n    A[First] --> B[Second] --> C[Third]";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.nodes.len(), 3);
        assert_eq!(parsed.edges.len(), 2);
        assert_eq!(parsed.edges[0].source, "A");
        assert_eq!(parsed.edges[0].target, "B");
        assert_eq!(parsed.edges[1].source, "B");
        assert_eq!(parsed.edges[1].target, "C");
    }

    #[test]
    fn parse_chained_edges_with_labels() {
        let input = "graph TD\n    A -->|yes| B -->|no| C";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.edges.len(), 2);
        assert_eq!(parsed.edges[0].label, Some("yes".into()));
        assert_eq!(parsed.edges[1].label, Some("no".into()));
    }

    #[test]
    fn unclosed_subgraph_auto_closed() {
        let input = "graph TD\n    subgraph Test\n        A --> B";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.subgraphs.len(), 1);
        assert_eq!(parsed.subgraphs[0].id, "Test");
        assert_eq!(parsed.subgraphs[0].node_ids.len(), 2);
    }

    #[test]
    fn malformed_annotation_skipped() {
        let input = "graph TD\n    A --> B\n    %% @TODO fix this later";
        let parsed = parse(input).unwrap();
        // Malformed annotation (no colon) is silently skipped
        assert_eq!(parsed.annotations.len(), 0);
    }

    #[test]
    fn comments_ignored() {
        let input = "graph TD\n    %% This is a comment\n    A --> B\n    %% Another comment";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.nodes.len(), 2);
        assert_eq!(parsed.annotations.len(), 0);
    }
}
