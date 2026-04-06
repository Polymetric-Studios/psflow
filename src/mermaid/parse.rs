use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Byte range in the source text (half-open: [from, to)).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Span {
    pub from: usize,
    pub to: usize,
}

impl Span {
    pub fn new(from: usize, to: usize) -> Self {
        Self { from, to }
    }

    /// Extend this span to include another span.
    pub fn extend(&mut self, other: Span) {
        if other.from < self.from {
            self.from = other.from;
        }
        if other.to > self.to {
            self.to = other.to;
        }
    }
}

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
    /// Byte range of the node's definition line in the source text.
    pub span: Span,
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
    /// Byte range from `subgraph` keyword to `end` keyword (inclusive).
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ParsedAnnotation {
    pub target_id: String,
    pub key: String,
    pub raw_value: String,
    /// Byte range of the entire annotation line in the source text.
    pub span: Span,
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
    let mut byte_offset: usize = 0;

    // State for multi-line annotation accumulation (>>> / <<<)
    let mut multiline_ann: Option<ParsedAnnotation> = None;
    let mut multiline_lines: Vec<String> = Vec::new();

    for (idx, raw_line) in input.lines().enumerate() {
        let line = raw_line.trim();
        let line_byte_start = byte_offset;
        let line_byte_end = byte_offset + raw_line.len();
        // Advance past this line + its newline character
        byte_offset = line_byte_end + if input[line_byte_end..].starts_with('\n') { 1 } else if input[line_byte_end..].starts_with("\r\n") { 2 } else { 0 };

        if line.is_empty() {
            continue;
        }
        let line_num = idx + 1;
        let line_span = Span::new(line_byte_start, line_byte_end);

        // Inside a multi-line annotation block: accumulate until <<<
        if let Some(ref mut ann) = multiline_ann {
            if let Some(body) = line.strip_prefix("%%") {
                let body = body.strip_prefix(' ').unwrap_or(body);
                if body.trim() == "<<<" {
                    // Close the block: join accumulated lines, push annotation
                    let mut ann = multiline_ann.take().unwrap();
                    ann.raw_value = multiline_lines.join("\n");
                    ann.span.extend(line_span);
                    result.annotations.push(ann);
                    multiline_lines.clear();
                } else {
                    multiline_lines.push(body.to_string());
                }
            } else if !line.is_empty() {
                // Non-comment line inside a >>> block is an error
                return Err(super::MermaidError::Parse {
                    line: line_num,
                    message: format!(
                        "non-comment line inside >>> block for @{} {}: {:?}",
                        ann.target_id, ann.key, line
                    ),
                });
            }
            continue;
        }

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
                span: line_span,
            });
            continue;
        }

        // Subgraph end
        if line == "end" {
            if let Some(mut sg) = sg_stack.pop() {
                sg.span.extend(line_span);
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
            if let Some(mut ann) = parse_annotation(rest) {
                // Check if this annotation opens a multi-line block
                if ann.raw_value == ">>>" {
                    ann.span = line_span;
                    ann.raw_value.clear();
                    multiline_ann = Some(ann);
                    multiline_lines.clear();
                } else {
                    ann.span = line_span;
                    result.annotations.push(ann);
                }
            }
            continue;
        }

        // Comment
        if line.starts_with("%%") {
            continue;
        }

        // Try edge declaration (supports chains: A --> B --> C)
        if let Some((nodes, edges)) = parse_edge_line(line) {
            for mut node in nodes {
                node.span = line_span;
                register_node(&mut result.nodes, &mut sg_stack, node);
            }
            result.edges.extend(edges);
            continue;
        }

        // Try standalone node declaration: A[Label]
        if let Some(mut node) = parse_node_ref(line) {
            node.span = line_span;
            register_node(&mut result.nodes, &mut sg_stack, node);
        }
    }

    // Error on unclosed multi-line annotation blocks
    if let Some(ann) = multiline_ann {
        return Err(super::MermaidError::Parse {
            line: input[..ann.span.from].lines().count() + 1,
            message: format!(
                "unclosed >>> block for @{} {} (missing <<<)",
                ann.target_id, ann.key
            ),
        });
    }

    // Auto-close unclosed subgraphs — span extends to end of input
    let eof_span = Span::new(input.len(), input.len());
    while let Some(mut sg) = sg_stack.pop() {
        sg.span.extend(eof_span);
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
        span: Span::default(),
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
            span: Span::default(),
        });
    }

    let rest = &s[id_end..];
    let (label, shape) = parse_shape_and_label(rest)?;

    Some(ParsedNode {
        id: id.to_string(),
        label,
        shape,
        span: Span::default(),
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

    #[test]
    fn node_spans_point_to_definition_line() {
        let input = "graph TD\n    A[Fetch] --> B[Process]\n    B --> C[Store]";
        let parsed = parse(input).unwrap();
        // Line 2: "    A[Fetch] --> B[Process]" starts at byte 9
        let a_span = parsed.nodes["A"].span;
        assert_eq!(&input[a_span.from..a_span.to], "    A[Fetch] --> B[Process]");
        // B appears on the same edge line, so its span covers the same line
        assert_eq!(parsed.nodes["B"].span, a_span);
        // Line 3: "    B --> C[Store]" — but B already registered from line 2
        let c_span = parsed.nodes["C"].span;
        assert_eq!(&input[c_span.from..c_span.to], "    B --> C[Store]");
    }

    #[test]
    fn annotation_spans_point_to_annotation_line() {
        let input = "graph TD\n    A --> B\n\n    %% @A handler: fetch\n    %% @A config.url: \"https://x.com\"";
        let parsed = parse(input).unwrap();
        let ann0 = &parsed.annotations[0];
        assert_eq!(ann0.target_id, "A");
        assert_eq!(&input[ann0.span.from..ann0.span.to], "    %% @A handler: fetch");
        let ann1 = &parsed.annotations[1];
        assert_eq!(
            &input[ann1.span.from..ann1.span.to],
            "    %% @A config.url: \"https://x.com\""
        );
    }

    #[test]
    fn subgraph_span_covers_start_to_end() {
        let input = "graph TD\n    subgraph Init [\"parallel: setup\"]\n        A --> B\n    end";
        let parsed = parse(input).unwrap();
        let sg = &parsed.subgraphs[0];
        // Span covers from subgraph line through end line
        let text = &input[sg.span.from..sg.span.to];
        assert!(text.starts_with("    subgraph Init"));
        assert!(text.ends_with("end"));
        assert_eq!(sg.span.from, 9); // "graph TD\n" = 9 bytes
        assert_eq!(sg.span.to, input.len());
    }

    #[test]
    fn multiline_annotation_basic() {
        let input = "\
graph TD
    A[Plan] --> B[Build]

    %% @A config.prompt: >>>
    %%   Plan the feature: {inputs.description}
    %%
    %%   Consider architecture.
    %% <<<
";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.annotations.len(), 1);
        let ann = &parsed.annotations[0];
        assert_eq!(ann.target_id, "A");
        assert_eq!(ann.key, "config.prompt");
        assert_eq!(
            ann.raw_value,
            "  Plan the feature: {inputs.description}\n\n  Consider architecture."
        );
    }

    #[test]
    fn multiline_annotation_single_line_body() {
        let input = "\
graph TD
    A --> B
    %% @A config.prompt: >>>
    %% Just one line.
    %% <<<
";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.annotations.len(), 1);
        assert_eq!(parsed.annotations[0].raw_value, "Just one line.");
    }

    #[test]
    fn multiline_annotation_empty_block() {
        let input = "\
graph TD
    A --> B
    %% @A config.prompt: >>>
    %% <<<
";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.annotations.len(), 1);
        assert_eq!(parsed.annotations[0].raw_value, "");
    }

    #[test]
    fn multiline_annotation_mixed_with_single_line() {
        let input = "\
graph TD
    A --> B

    %% @A handler: agentic
    %% @A config.prompt: >>>
    %%   Multi-line prompt
    %%   with two lines.
    %% <<<
    %% @A config.agent: athena
";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.annotations.len(), 3);
        assert_eq!(parsed.annotations[0].key, "handler");
        assert_eq!(parsed.annotations[0].raw_value, "agentic");
        assert_eq!(parsed.annotations[1].key, "config.prompt");
        assert_eq!(
            parsed.annotations[1].raw_value,
            "  Multi-line prompt\n  with two lines."
        );
        assert_eq!(parsed.annotations[2].key, "config.agent");
        assert_eq!(parsed.annotations[2].raw_value, "athena");
    }

    #[test]
    fn multiline_annotation_span_covers_block() {
        let input = "\
graph TD
    A --> B

    %% @A config.prompt: >>>
    %% Line one.
    %% Line two.
    %% <<<
";
        let parsed = parse(input).unwrap();
        let ann = &parsed.annotations[0];
        let text = &input[ann.span.from..ann.span.to];
        assert!(text.contains(">>>"));
        assert!(text.contains("<<<"));
    }

    #[test]
    fn unclosed_multiline_block_is_error() {
        let input = "\
graph TD
    A --> B
    %% @A config.prompt: >>>
    %% Some content
    %% More content
";
        let err = parse(input).unwrap_err();
        match err {
            super::super::MermaidError::Parse { message, .. } => {
                assert!(message.contains("unclosed >>>"), "got: {message}");
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn non_comment_line_inside_multiline_block_is_error() {
        let input = "\
graph TD
    A --> B
    %% @A config.prompt: >>>
    %% Line one
    C[Oops] --> D
    %% <<<
";
        let err = parse(input).unwrap_err();
        match err {
            super::super::MermaidError::Parse { message, .. } => {
                assert!(
                    message.contains("non-comment line"),
                    "got: {message}"
                );
            }
            other => panic!("expected Parse error, got {other:?}"),
        }
    }

    #[test]
    fn multiple_sequential_multiline_blocks() {
        let input = "\
graph TD
    A --> B

    %% @A config.prompt: >>>
    %% Prompt for A
    %% <<<
    %% @B config.prompt: >>>
    %% Prompt for B
    %% with two lines
    %% <<<
";
        let parsed = parse(input).unwrap();
        assert_eq!(parsed.annotations.len(), 2);
        assert_eq!(parsed.annotations[0].target_id, "A");
        assert_eq!(parsed.annotations[0].raw_value, "Prompt for A");
        assert_eq!(parsed.annotations[1].target_id, "B");
        assert_eq!(
            parsed.annotations[1].raw_value,
            "Prompt for B\nwith two lines"
        );
    }
}

#[cfg(test)]
mod fuzz_tests {
    use super::*;
    use proptest::prelude::*;

    // Parser must never panic on arbitrary input
    proptest! {
        #[test]
        fn parse_never_panics(input in "\\PC{0,500}") {
            let _ = parse(&input);
        }

        #[test]
        fn parse_never_panics_with_mermaid_chars(
            input in prop::collection::vec(
                prop::sample::select(vec![
                    "graph TD\n", "flowchart LR\n",
                    "A", "B", "C", "D", "node1",
                    "[", "]", "(", ")", "{", "}", "((", "))", "[[", "]]",
                    " --> ", " --- ", " -.-> ", " ==> ",
                    "|label|", "|yes|", "|no|",
                    "\n    subgraph S\n", "\n    end\n",
                    "\n    %% @A handler: test\n",
                    "\n    %% comment\n",
                    " ", "\n", "\t",
                ]),
                0..30
            )
        ) {
            let text: String = input.into_iter().collect();
            let _ = parse(&text);
        }

        #[test]
        fn parse_node_ref_never_panics(input in "\\PC{0,200}") {
            let _ = parse_node_ref(&input);
        }

        #[test]
        fn parse_shape_and_label_never_panics(input in "\\PC{0,200}") {
            let _ = parse_shape_and_label(&input);
        }

        #[test]
        fn parse_annotation_never_panics(input in "\\PC{0,200}") {
            let _ = parse_annotation(&input);
        }

        #[test]
        fn valid_graph_always_parses(
            direction in prop::sample::select(vec!["TD", "LR", "BT", "RL"]),
            node_count in 2..10usize,
        ) {
            let mut lines = vec![format!("graph {direction}")];
            let ids: Vec<String> = (0..node_count).map(|i| format!("N{i}")).collect();
            // Chain all nodes
            for i in 0..node_count - 1 {
                lines.push(format!("    {}[Node {}] --> {}[Node {}]", ids[i], i, ids[i+1], i+1));
            }
            let input = lines.join("\n");
            let result = parse(&input).unwrap();
            prop_assert_eq!(result.edges.len(), node_count - 1);
            prop_assert!(result.nodes.len() >= node_count);
        }
    }
}
