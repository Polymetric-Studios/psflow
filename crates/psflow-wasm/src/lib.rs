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

/// Parse a .mmd source string and return node/annotation ranges for the debugger.
#[wasm_bindgen]
pub fn parse_mmd(source: &str) -> Result<ParseResult, JsError> {
    let parsed = match parse::parse(source) {
        Ok(p) => p,
        Err(e) => {
            return Ok(ParseResult {
                nodes: vec![],
                subgraphs: vec![],
                errors: vec![e.to_string()],
            });
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

    Ok(ParseResult {
        nodes,
        subgraphs,
        errors: vec![],
    })
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

/// Parse a JSON execution trace string into debugger-friendly events.
#[wasm_bindgen]
pub fn parse_trace(json: &str) -> Result<TraceResult, JsError> {
    let trace: TraceJson =
        serde_json::from_str(json).map_err(|e| JsError::new(&e.to_string()))?;

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
