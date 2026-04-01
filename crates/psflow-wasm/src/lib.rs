use psflow::mermaid::parse::{self, Span};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tsify_next::Tsify;
use wasm_bindgen::prelude::*;

// --- Parse API types ---

/// Result of parsing a .mmd file — node ranges and annotation ranges for the debugger.
#[derive(Debug, Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub struct ParseResult {
    pub nodes: Vec<NodeRange>,
    pub subgraphs: Vec<SubgraphRange>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub struct NodeRange {
    pub id: String,
    pub label: String,
    pub definition: SpanDto,
    pub annotations: Vec<AnnotationRange>,
}

#[derive(Debug, Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub struct AnnotationRange {
    pub key: String,
    pub value: String,
    pub span: SpanDto,
}

#[derive(Debug, Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub struct SubgraphRange {
    pub id: String,
    pub label: Option<String>,
    pub node_ids: Vec<String>,
    pub span: SpanDto,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub struct SpanDto {
    pub from: usize,
    pub to: usize,
}

impl From<Span> for SpanDto {
    fn from(s: Span) -> Self {
        SpanDto { from: s.from, to: s.to }
    }
}

/// Internal parsing logic, testable without wasm_bindgen.
fn parse_mmd_internal(source: &str) -> ParseResult {
    let parsed = match parse::parse(source) {
        Ok(p) => p,
        Err(e) => {
            return ParseResult {
                nodes: vec![],
                subgraphs: vec![],
                errors: vec![e.to_string()],
            };
        }
    };

    // Group annotations by target node ID
    let mut annotations_by_node: HashMap<String, Vec<AnnotationRange>> = HashMap::new();
    for ann in &parsed.annotations {
        annotations_by_node
            .entry(ann.target_id.clone())
            .or_default()
            .push(AnnotationRange {
                key: ann.key.clone(),
                value: ann.raw_value.clone(),
                span: ann.span.into(),
            });
    }

    // Build node ranges
    let mut nodes: Vec<NodeRange> = parsed
        .nodes
        .values()
        .map(|node| NodeRange {
            id: node.id.clone(),
            label: node.label.clone(),
            definition: node.span.into(),
            annotations: annotations_by_node.remove(&node.id).unwrap_or_default(),
        })
        .collect();
    nodes.sort_by_key(|n| n.definition.from);

    let subgraphs = collect_subgraphs(&parsed.subgraphs);

    ParseResult {
        nodes,
        subgraphs,
        errors: vec![],
    }
}

/// Parse a .mmd source string and return node/annotation ranges for the debugger.
#[wasm_bindgen]
pub fn parse_mmd(source: &str) -> Result<ParseResult, JsError> {
    Ok(parse_mmd_internal(source))
}

fn collect_subgraphs(sgs: &[parse::ParsedSubgraph]) -> Vec<SubgraphRange> {
    sgs.iter()
        .map(|sg| SubgraphRange {
            id: sg.id.clone(),
            label: sg.label.clone(),
            node_ids: sg.node_ids.clone(),
            span: sg.span.into(),
        })
        .collect()
}

// --- Trace API types ---
// Defined locally to avoid depending on psflow's runtime-gated execute module.
// These mirror the JSON format of ExecutionTrace/TraceRecord for deserialization.

#[derive(Debug, Deserialize)]
struct TraceJson {
    records: Vec<TraceRecordJson>,
    elapsed: DurationJson,
}

#[derive(Debug, Deserialize)]
struct TraceRecordJson {
    node_id: String,
    order: usize,
    state: String,
    elapsed: Option<DurationJson>,
    error: Option<serde_json::Value>,
    outputs: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct DurationJson {
    secs: u64,
    nanos: u32,
}

impl DurationJson {
    fn as_ms(&self) -> f64 {
        (self.secs as f64) * 1000.0 + (self.nanos as f64) / 1_000_000.0
    }
}

/// A single trace event for the debugger timeline.
#[derive(Debug, Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub struct TraceEvent {
    pub node_id: String,
    pub state: String,
    pub order: usize,
    pub elapsed_ms: Option<f64>,
    pub error: Option<String>,
    pub outputs_json: Option<String>,
}

/// Parsed trace result.
#[derive(Debug, Serialize, Deserialize, Tsify)]
#[tsify(into_wasm_abi, from_wasm_abi)]
pub struct TraceResult {
    pub events: Vec<TraceEvent>,
    pub total_elapsed_ms: f64,
}

/// Internal trace parsing logic, testable without wasm_bindgen.
fn parse_trace_internal(json: &str) -> Result<TraceResult, String> {
    let trace: TraceJson =
        serde_json::from_str(json).map_err(|e| e.to_string())?;

    let events = trace
        .records
        .iter()
        .map(|r| TraceEvent {
            node_id: r.node_id.clone(),
            state: r.state.to_lowercase(),
            order: r.order,
            elapsed_ms: r.elapsed.as_ref().map(|d| d.as_ms()),
            error: r.error.as_ref().map(|e| serde_json::to_string(e).unwrap_or_default()),
            outputs_json: r.outputs.as_ref().map(|o| serde_json::to_string(o).unwrap_or_default()),
        })
        .collect();

    Ok(TraceResult {
        events,
        total_elapsed_ms: trace.elapsed.as_ms(),
    })
}

/// Parse a JSON execution trace string into debugger-friendly events.
#[wasm_bindgen]
pub fn parse_trace(json: &str) -> Result<TraceResult, JsError> {
    parse_trace_internal(json).map_err(|e| JsError::new(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // 5.T.1 — Parser parity: demo.mmd content
    // ---------------------------------------------------------------

    const DEMO_MMD: &str = "\
graph TD
    A[Generate Data] --> B[Double It]
    B --> C{Big Enough?}
    C -->|yes| D[Format Output]
    C -->|no| E[Too Small]

    %% @A handler: rhai
    %% @A config.script: \"#{ value: 42, label: \\\"hello world\\\" }\"
    %% @A outputs.value: \"I64\"
    %% @A outputs.label: \"String\"

    %% @B handler: rhai
    %% @B config.script: \"let v = inputs.value * 2; #{ doubled: v }\"
    %% @B inputs.value: \"I64\"
    %% @B outputs.doubled: \"I64\"

    %% @C handler: branch
    %% @C config.guard: \"inputs.doubled > 50\"

    %% @D handler: rhai
    %% @D config.script: \"#{ message: \\\"Result is \\\" + inputs.doubled.to_string() }\"
    %% @D inputs.doubled: \"I64\"
    %% @D outputs.message: \"String\"

    %% @E handler: log
    %% @E config.message: \"Value was too small\"
";

    #[test]
    fn parse_demo_returns_five_nodes() {
        let result = parse_mmd_internal(DEMO_MMD);
        assert!(result.errors.is_empty(), "expected no errors: {:?}", result.errors);
        assert_eq!(result.nodes.len(), 5);
    }

    #[test]
    fn parse_demo_node_ids_and_labels() {
        let result = parse_mmd_internal(DEMO_MMD);
        let ids: Vec<&str> = result.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"A"), "missing node A");
        assert!(ids.contains(&"B"), "missing node B");
        assert!(ids.contains(&"C"), "missing node C");
        assert!(ids.contains(&"D"), "missing node D");
        assert!(ids.contains(&"E"), "missing node E");

        let find = |id: &str| result.nodes.iter().find(|n| n.id == id).unwrap();
        assert_eq!(find("A").label, "Generate Data");
        assert_eq!(find("B").label, "Double It");
        assert_eq!(find("C").label, "Big Enough?");
        assert_eq!(find("D").label, "Format Output");
        assert_eq!(find("E").label, "Too Small");
    }

    #[test]
    fn parse_demo_annotations_grouped_by_node() {
        let result = parse_mmd_internal(DEMO_MMD);
        let find = |id: &str| result.nodes.iter().find(|n| n.id == id).unwrap();

        // Node A: handler, config.script, outputs.value, outputs.label
        let a = find("A");
        assert_eq!(a.annotations.len(), 4, "A should have 4 annotations");
        let a_keys: Vec<&str> = a.annotations.iter().map(|a| a.key.as_str()).collect();
        assert!(a_keys.contains(&"handler"));
        assert!(a_keys.contains(&"config.script"));
        assert!(a_keys.contains(&"outputs.value"));
        assert!(a_keys.contains(&"outputs.label"));

        // Check a specific annotation value
        let handler_ann = a.annotations.iter().find(|a| a.key == "handler").unwrap();
        assert_eq!(handler_ann.value, "rhai");

        // Node B: handler, config.script, inputs.value, outputs.doubled
        let b = find("B");
        assert_eq!(b.annotations.len(), 4, "B should have 4 annotations");

        // Node C: handler, config.guard
        let c = find("C");
        assert_eq!(c.annotations.len(), 2, "C should have 2 annotations");

        // Node D: handler, config.script, inputs.doubled, outputs.message
        let d = find("D");
        assert_eq!(d.annotations.len(), 4, "D should have 4 annotations");

        // Node E: handler, config.message
        let e = find("E");
        assert_eq!(e.annotations.len(), 2, "E should have 2 annotations");
    }

    #[test]
    fn parse_demo_annotation_values() {
        let result = parse_mmd_internal(DEMO_MMD);
        let find = |id: &str| result.nodes.iter().find(|n| n.id == id).unwrap();

        let c = find("C");
        let guard = c.annotations.iter().find(|a| a.key == "config.guard").unwrap();
        assert_eq!(guard.value, "\"inputs.doubled > 50\"");

        let e = find("E");
        let msg = e.annotations.iter().find(|a| a.key == "config.message").unwrap();
        assert_eq!(msg.value, "\"Value was too small\"");

        let a = find("A");
        let ov = a.annotations.iter().find(|a| a.key == "outputs.value").unwrap();
        assert_eq!(ov.value, "\"I64\"");
    }

    #[test]
    fn parse_demo_nodes_sorted_by_definition_offset() {
        let result = parse_mmd_internal(DEMO_MMD);
        let offsets: Vec<usize> = result.nodes.iter().map(|n| n.definition.from).collect();
        for w in offsets.windows(2) {
            assert!(w[0] <= w[1], "nodes should be sorted by definition.from: {:?}", offsets);
        }
    }

    // ---------------------------------------------------------------
    // 5.T.2 — Range mapping
    // ---------------------------------------------------------------

    #[test]
    fn definition_span_covers_correct_line() {
        let result = parse_mmd_internal(DEMO_MMD);

        // The parser assigns the full line span (including leading whitespace) to
        // node definitions. Verify the spanned text contains the node ID+label.
        for node in &result.nodes {
            let span_text = &DEMO_MMD[node.definition.from..node.definition.to];
            assert!(
                span_text.contains(&node.id),
                "Node {} definition span should contain its ID, got: '{}'",
                node.id, span_text
            );
            // Span should be a single line (no embedded newlines)
            assert!(
                !span_text.contains('\n'),
                "Node {} definition span should not cross lines",
                node.id
            );
        }
    }

    #[test]
    fn definition_spans_have_ascending_from_offsets() {
        // Nodes A..E appear in source order on lines 2..5;
        // verify the sorted order matches source order.
        let result = parse_mmd_internal(DEMO_MMD);
        let ids: Vec<&str> = result.nodes.iter().map(|n| n.id.as_str()).collect();
        // After sorting by definition.from the first node defined should be A.
        assert_eq!(ids[0], "A", "first node by byte offset should be A");
    }

    #[test]
    fn annotation_span_bounds_correct_line() {
        let result = parse_mmd_internal(DEMO_MMD);
        let find = |id: &str| result.nodes.iter().find(|n| n.id == id).unwrap();

        let a = find("A");
        let handler = a.annotations.iter().find(|a| a.key == "handler").unwrap();
        let span_text = &DEMO_MMD[handler.span.from..handler.span.to];
        assert!(
            span_text.contains("@A handler"),
            "annotation span should contain '@A handler', got: '{}'",
            span_text
        );
        // Span should not cross line boundaries (no newlines)
        assert!(
            !span_text.contains('\n'),
            "annotation span should not span multiple lines"
        );
    }

    #[test]
    fn all_annotation_spans_within_source_bounds() {
        let result = parse_mmd_internal(DEMO_MMD);
        let source_len = DEMO_MMD.len();
        for node in &result.nodes {
            for ann in &node.annotations {
                assert!(
                    ann.span.from <= ann.span.to,
                    "annotation span.from ({}) > span.to ({}) for {}::{}",
                    ann.span.from, ann.span.to, node.id, ann.key
                );
                assert!(
                    ann.span.to <= source_len,
                    "annotation span.to ({}) exceeds source length ({}) for {}::{}",
                    ann.span.to, source_len, node.id, ann.key
                );
            }
        }
    }

    // ---------------------------------------------------------------
    // Edge cases
    // ---------------------------------------------------------------

    #[test]
    fn parse_empty_source() {
        let result = parse_mmd_internal("");
        // Empty input yields no nodes and no errors (the parser tolerates it).
        assert!(result.nodes.is_empty());
        assert!(result.subgraphs.is_empty());
    }

    #[test]
    fn parse_graph_with_no_annotations() {
        let source = "graph TD\n    X[Hello] --> Y[World]\n";
        let result = parse_mmd_internal(source);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.nodes.len(), 2);
        for node in &result.nodes {
            assert!(node.annotations.is_empty(), "node {} should have no annotations", node.id);
        }
    }

    #[test]
    fn parse_adjacent_nodes_same_line() {
        // Two node definitions on a single edge line
        let source = "graph LR\n    P[First] --> Q[Second]\n";
        let result = parse_mmd_internal(source);
        assert!(result.errors.is_empty(), "errors: {:?}", result.errors);
        assert_eq!(result.nodes.len(), 2);
        let ids: Vec<&str> = result.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains(&"P"));
        assert!(ids.contains(&"Q"));
    }

    // ---------------------------------------------------------------
    // 5.T.1 — Trace deserialization round-trip
    // ---------------------------------------------------------------

    #[test]
    fn parse_trace_round_trip() {
        let json = r#"{
            "records": [
                {
                    "node_id": "A",
                    "order": 0,
                    "state": "Completed",
                    "elapsed": { "secs": 0, "nanos": 1500000 },
                    "error": null,
                    "outputs": { "value": 42 }
                },
                {
                    "node_id": "B",
                    "order": 1,
                    "state": "Running",
                    "elapsed": { "secs": 1, "nanos": 0 },
                    "error": null,
                    "outputs": null
                },
                {
                    "node_id": "B",
                    "order": 2,
                    "state": "Failed",
                    "elapsed": { "secs": 1, "nanos": 250000000 },
                    "error": "something broke",
                    "outputs": null
                }
            ],
            "elapsed": { "secs": 2, "nanos": 500000000 }
        }"#;

        let result = parse_trace_internal(json).expect("should parse valid trace JSON");

        assert_eq!(result.events.len(), 3);
        assert!((result.total_elapsed_ms - 2500.0).abs() < 0.01);

        // First event
        let e0 = &result.events[0];
        assert_eq!(e0.node_id, "A");
        assert_eq!(e0.state, "completed");
        assert_eq!(e0.order, 0);
        assert!((e0.elapsed_ms.unwrap() - 1.5).abs() < 0.01);
        assert!(e0.error.is_none());
        assert_eq!(e0.outputs_json.as_deref(), Some("{\"value\":42}"));

        // Second event
        let e1 = &result.events[1];
        assert_eq!(e1.node_id, "B");
        assert_eq!(e1.state, "running");
        assert_eq!(e1.order, 1);
        assert!((e1.elapsed_ms.unwrap() - 1000.0).abs() < 0.01);
        assert!(e1.outputs_json.is_none());

        // Third event with error
        let e2 = &result.events[2];
        assert_eq!(e2.node_id, "B");
        assert_eq!(e2.state, "failed");
        assert_eq!(e2.order, 2);
        assert!(e2.error.is_some());
    }

    #[test]
    fn parse_trace_invalid_json_returns_error() {
        let result = parse_trace_internal("not valid json");
        assert!(result.is_err());
    }

    #[test]
    fn parse_trace_empty_records() {
        let json = r#"{ "records": [], "elapsed": { "secs": 0, "nanos": 0 } }"#;
        let result = parse_trace_internal(json).expect("should parse empty trace");
        assert!(result.events.is_empty());
        assert!((result.total_elapsed_ms - 0.0).abs() < 0.001);
    }

    #[test]
    fn parse_trace_state_lowercased() {
        let json = r#"{
            "records": [{
                "node_id": "X",
                "order": 0,
                "state": "COMPLETED",
                "elapsed": null,
                "error": null,
                "outputs": null
            }],
            "elapsed": { "secs": 0, "nanos": 0 }
        }"#;
        let result = parse_trace_internal(json).unwrap();
        assert_eq!(result.events[0].state, "completed");
        assert!(result.events[0].elapsed_ms.is_none());
    }
}
