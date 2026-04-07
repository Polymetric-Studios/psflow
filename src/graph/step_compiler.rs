//! Compile JSON step definitions into graph topology.
//!
//! Structural types (parallel, serial, loop, conditional) are expanded into
//! subgraph patterns with control nodes. Leaf types become single nodes with
//! `handler = step["type"]` — callers register their own handlers for those.

use super::{Graph, Subgraph, SubgraphDirective};
use crate::graph::node::Node;
use crate::graph::port::Port;
use crate::graph::types::PortType;
use serde_json::Value;

// ---------------------------------------------------------------------------
// Handler name constants — prefixed to avoid collision with user handlers
// ---------------------------------------------------------------------------

pub const HANDLER_FORK: &str = "step:fork";
pub const HANDLER_JOIN: &str = "step:join";
pub const HANDLER_LOOP_START: &str = "step:loop-start";
pub const HANDLER_LOOP_END: &str = "step:loop-end";
pub const HANDLER_BRANCH: &str = "step:branch";
pub const HANDLER_MERGE: &str = "step:merge";

/// Known structural step types that the compiler interprets.
const STRUCTURAL_TYPES: &[&str] = &["parallel", "serial", "loop", "conditional"];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Compile a list of JSON step definitions into a psflow [`Graph`].
///
/// Each step must have an `"id"` (string) and `"type"` (string) field.
/// Structural types (`parallel`, `serial`, `loop`, `conditional`) are expanded
/// into subgraph patterns. All other types become leaf nodes with
/// `handler = step["type"]` and `config = step.clone()`.
pub fn compile_steps(name: &str, steps: &[Value]) -> Result<Graph, String> {
    let mut graph = Graph::new();
    graph.metadata_mut().name = Some(name.to_owned());
    process_steps(&mut graph, steps, "", None)?;
    Ok(graph)
}

// ---------------------------------------------------------------------------
// Node construction helpers
// ---------------------------------------------------------------------------

fn any_port(name: &str) -> Port {
    Port::new(name, PortType::Any)
}

fn make_node(id: &str, label: &str, handler: &str) -> Node {
    Node::new(id, label)
        .with_handler(handler)
        .with_input(any_port("in"))
        .with_output(any_port("out"))
}

fn add_node(graph: &mut Graph, node: Node) -> Result<(), String> {
    graph.add_node(node).map_err(|e| e.to_string())
}

fn connect(graph: &mut Graph, from: &str, to: &str, label: Option<String>) -> Result<(), String> {
    graph
        .add_edge(
            &from.into(),
            "out",
            &to.into(),
            "in",
            label,
        )
        .map_err(|e| e.to_string())
}

fn connect_named(
    graph: &mut Graph,
    from: &str,
    to: &str,
    target_port: &str,
    label: Option<String>,
) -> Result<(), String> {
    graph
        .add_edge(
            &from.into(),
            "out",
            &to.into(),
            target_port,
            label,
        )
        .map_err(|e| e.to_string())
}

fn namespaced_id(prefix: &str, raw_id: &str) -> String {
    if prefix.is_empty() {
        raw_id.to_owned()
    } else {
        format!("{prefix}/{raw_id}")
    }
}

// ---------------------------------------------------------------------------
// Step processing
// ---------------------------------------------------------------------------

/// Walk a list of steps, chaining them sequentially. Returns the last node ID.
fn process_steps(
    graph: &mut Graph,
    steps: &[Value],
    prefix: &str,
    previous: Option<&str>,
) -> Result<Option<String>, String> {
    let mut prev: Option<String> = previous.map(str::to_owned);

    for step in steps {
        let last = process_step(graph, step, prefix, prev.as_deref())?;
        if let Some(id) = last {
            prev = Some(id);
        }
    }

    Ok(prev)
}

/// Process a single step. Returns the last node ID produced.
fn process_step(
    graph: &mut Graph,
    step: &Value,
    prefix: &str,
    previous: Option<&str>,
) -> Result<Option<String>, String> {
    let raw_id = step["id"]
        .as_str()
        .ok_or_else(|| "step missing 'id' field".to_owned())?;

    if raw_id.contains('/') {
        return Err(format!(
            "step id '{raw_id}' must not contain '/' (reserved as namespace separator)"
        ));
    }

    let step_type = step["type"]
        .as_str()
        .ok_or_else(|| format!("step '{raw_id}' missing 'type' field"))?;

    let node_id = namespaced_id(prefix, raw_id);

    match step_type {
        "parallel" => process_parallel(graph, step, &node_id, previous),
        "serial" => process_serial(graph, step, &node_id, previous),
        "loop" => process_loop(graph, step, &node_id, previous),
        "conditional" => process_conditional(graph, step, &node_id, previous),
        _ => process_leaf(graph, step, &node_id, step_type, previous),
    }
}

// -- Leaf nodes --

fn process_leaf(
    graph: &mut Graph,
    step: &Value,
    node_id: &str,
    step_type: &str,
    previous: Option<&str>,
) -> Result<Option<String>, String> {
    let mut node = make_node(node_id, node_id, step_type);
    node.config = step.clone();
    add_node(graph, node)?;

    if let Some(prev) = previous {
        connect(graph, prev, node_id, None)?;
    }

    Ok(Some(node_id.to_owned()))
}

// -- Parallel --

fn process_parallel(
    graph: &mut Graph,
    step: &Value,
    node_id: &str,
    previous: Option<&str>,
) -> Result<Option<String>, String> {
    let fork_id = format!("{node_id}/fork");
    let join_id = format!("{node_id}/join");

    let mut fork_node = make_node(&fork_id, &format!("{node_id} fork"), HANDLER_FORK);
    fork_node.config = step.clone();
    add_node(graph, fork_node)?;

    if let Some(prev) = previous {
        connect(graph, prev, &fork_id, None)?;
    }

    // Add join before sub-steps so we can connect into it
    let mut join_node = make_node(&join_id, &format!("{node_id} join"), HANDLER_JOIN);
    join_node.config = step.clone();
    add_node(graph, join_node)?;

    let sub_steps = step["steps"]
        .as_array()
        .ok_or_else(|| format!("parallel step '{node_id}' missing 'steps' array"))?;

    let mut branch_node_ids = Vec::new();

    for sub in sub_steps {
        let sub_raw_id = sub["id"]
            .as_str()
            .ok_or_else(|| "parallel sub-step missing 'id'".to_owned())?;

        let sub_node_id = namespaced_id(node_id, sub_raw_id);
        let last = process_step(graph, sub, node_id, Some(&fork_id))?;
        if let Some(last_id) = last {
            branch_node_ids.push(last_id.clone().into());
            // Named target port so join receives each branch under a distinct key
            connect_named(graph, &last_id, &join_id, sub_raw_id, None)?;
        } else {
            branch_node_ids.push(sub_node_id.into());
        }
    }

    graph.add_subgraph(Subgraph {
        id: format!("sg-{node_id}"),
        label: format!("Parallel: {node_id}"),
        directive: SubgraphDirective::Parallel,
        nodes: branch_node_ids,
        children: vec![],
    });

    Ok(Some(join_id))
}

// -- Serial --

fn process_serial(
    graph: &mut Graph,
    step: &Value,
    node_id: &str,
    previous: Option<&str>,
) -> Result<Option<String>, String> {
    let sub_steps = step["steps"]
        .as_array()
        .ok_or_else(|| format!("serial step '{node_id}' missing 'steps' array"))?;

    process_steps(graph, sub_steps, node_id, previous)
}

// -- Loop --

fn process_loop(
    graph: &mut Graph,
    step: &Value,
    node_id: &str,
    previous: Option<&str>,
) -> Result<Option<String>, String> {
    let start_id = format!("{node_id}/loop-start");
    let end_id = format!("{node_id}/loop-end");

    let over_val = step["over"].as_str().unwrap_or("items");
    let mut start_node = make_node(&start_id, &format!("loop over {over_val}"), HANDLER_LOOP_START);
    start_node.config = step.clone();
    add_node(graph, start_node)?;

    if let Some(prev) = previous {
        connect(graph, prev, &start_id, None)?;
    }

    // Body: a single "step" or an array "steps"
    let last_body_id = if let Some(body_step) = step.get("step") {
        if body_step.is_object() {
            process_step(graph, body_step, node_id, Some(&start_id))?
        } else {
            None
        }
    } else if let Some(body_steps) = step["steps"].as_array() {
        process_steps(graph, body_steps, node_id, Some(&start_id))?
    } else {
        None
    };

    let end_anchor = last_body_id.as_deref().unwrap_or(&start_id);

    // Collect body node IDs for the subgraph.
    // NOTE: This collects ALL descendant nodes (including those inside nested
    // substructures like parallel groups). Nodes may therefore appear in multiple
    // subgraphs. The LoopController relies on this to reset all body nodes
    // between iterations regardless of nesting depth.
    let loop_body_ids: Vec<_> = graph
        .nodes()
        .filter(|n| {
            n.id.0.starts_with(&format!("{node_id}/"))
                && n.id.0 != start_id
                && n.id.0 != end_id
        })
        .map(|n| n.id.clone())
        .collect();

    let mut end_node = make_node(&end_id, &format!("{node_id} loop-end"), HANDLER_LOOP_END);
    end_node.config = step.clone();
    add_node(graph, end_node)?;

    connect(graph, end_anchor, &end_id, None)?;

    graph.add_subgraph(Subgraph {
        id: format!("sg-{node_id}"),
        label: format!("Loop: {node_id}"),
        directive: SubgraphDirective::Loop,
        nodes: loop_body_ids,
        children: vec![],
    });

    Ok(Some(end_id))
}

// -- Conditional --

fn process_conditional(
    graph: &mut Graph,
    step: &Value,
    node_id: &str,
    previous: Option<&str>,
) -> Result<Option<String>, String> {
    let branch_id = format!("{node_id}/branch");
    let merge_id = format!("{node_id}/merge");

    let condition = step["condition"].as_str().unwrap_or("condition");
    let mut branch_node = make_node(&branch_id, condition, HANDLER_BRANCH);
    branch_node.config = step.clone();
    add_node(graph, branch_node)?;

    if let Some(prev) = previous {
        connect(graph, prev, &branch_id, None)?;
    }

    // Then branch
    let then_prefix = format!("{node_id}/then");
    let then_last = if let Some(then_steps) = step["then"].as_array() {
        process_steps(graph, then_steps, &then_prefix, Some(&branch_id))?
    } else if let Some(then_step) = step.get("then") {
        if then_step.is_object() {
            process_step(graph, then_step, &then_prefix, Some(&branch_id))?
        } else {
            None
        }
    } else {
        None
    };

    // Else branch
    let else_prefix = format!("{node_id}/else");
    let else_last = if let Some(else_steps) = step["else"].as_array() {
        process_steps(graph, else_steps, &else_prefix, Some(&branch_id))?
    } else if let Some(else_step) = step.get("else") {
        if else_step.is_object() {
            process_step(graph, else_step, &else_prefix, Some(&branch_id))?
        } else {
            None
        }
    } else {
        None
    };

    let mut merge_node = make_node(&merge_id, &format!("{node_id} merge"), HANDLER_MERGE);
    merge_node.config = step.clone();
    add_node(graph, merge_node)?;

    // Connect then/else to merge with named ports
    if let Some(t) = then_last {
        connect_named(graph, &t, &merge_id, "then", Some("true".into()))?;
    } else {
        connect_named(graph, &branch_id, &merge_id, "then", Some("true".into()))?;
    }

    if let Some(e) = else_last {
        connect_named(graph, &e, &merge_id, "else", Some("false".into()))?;
    } else {
        connect_named(graph, &branch_id, &merge_id, "else", Some("false".into()))?;
    }

    Ok(Some(merge_id))
}

/// Returns true if `type_name` is a structural type that the compiler interprets.
pub fn is_structural_type(type_name: &str) -> bool {
    STRUCTURAL_TYPES.contains(&type_name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mermaid::export_mermaid;
    use serde_json::json;

    #[test]
    fn sequential_steps() {
        let steps = vec![
            json!({"id": "a", "type": "agent", "prompt": "do A"}),
            json!({"id": "b", "type": "agent", "prompt": "do B"}),
            json!({"id": "c", "type": "agent", "prompt": "do C"}),
        ];
        let graph = compile_steps("test", &steps).unwrap();
        assert_eq!(graph.node_count(), 3);
        assert_eq!(graph.edge_count(), 2); // a->b, b->c
        assert!(graph.node(&"a".into()).is_some());
        assert!(graph.node(&"c".into()).is_some());
    }

    #[test]
    fn single_step() {
        let steps = vec![json!({"id": "only", "type": "agent"})];
        let graph = compile_steps("test", &steps).unwrap();
        assert_eq!(graph.node_count(), 1);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn empty_steps() {
        let graph = compile_steps("test", &[]).unwrap();
        assert_eq!(graph.node_count(), 0);
        assert_eq!(graph.edge_count(), 0);
    }

    #[test]
    fn missing_id_error() {
        let steps = vec![json!({"type": "agent"})];
        let err = compile_steps("test", &steps).unwrap_err();
        assert!(err.contains("missing 'id'"));
    }

    #[test]
    fn missing_type_error() {
        let steps = vec![json!({"id": "a"})];
        let err = compile_steps("test", &steps).unwrap_err();
        assert!(err.contains("missing 'type'"));
    }

    #[test]
    fn leaf_node_handler_is_type() {
        let steps = vec![json!({"id": "x", "type": "my-custom-handler", "foo": "bar"})];
        let graph = compile_steps("test", &steps).unwrap();
        let node = graph.node(&"x".into()).unwrap();
        assert_eq!(node.handler.as_deref(), Some("my-custom-handler"));
        assert_eq!(node.config["foo"], "bar");
    }

    #[test]
    fn parallel_expansion() {
        let steps = vec![json!({
            "id": "par",
            "type": "parallel",
            "steps": [
                {"id": "w1", "type": "agent", "prompt": "A"},
                {"id": "w2", "type": "agent", "prompt": "B"},
            ]
        })];
        let graph = compile_steps("test", &steps).unwrap();
        // fork + w1 + w2 + join = 4
        assert_eq!(graph.node_count(), 4);
        // fork->w1, fork->w2, w1->join, w2->join = 4
        assert_eq!(graph.edge_count(), 4);
        assert_eq!(graph.subgraphs().len(), 1);
        assert_eq!(graph.subgraphs()[0].directive, SubgraphDirective::Parallel);

        // Check handler names
        assert_eq!(
            graph.node(&"par/fork".into()).unwrap().handler.as_deref(),
            Some(HANDLER_FORK)
        );
        assert_eq!(
            graph.node(&"par/join".into()).unwrap().handler.as_deref(),
            Some(HANDLER_JOIN)
        );
    }

    #[test]
    fn loop_expansion() {
        let steps = vec![json!({
            "id": "lp",
            "type": "loop",
            "over": "items",
            "step": {"id": "body", "type": "agent", "prompt": "process"}
        })];
        let graph = compile_steps("test", &steps).unwrap();
        // loop-start + body + loop-end = 3
        assert_eq!(graph.node_count(), 3);
        assert!(graph.node(&"lp/loop-start".into()).is_some());
        assert!(graph.node(&"lp/body".into()).is_some());
        assert!(graph.node(&"lp/loop-end".into()).is_some());
        assert_eq!(graph.subgraphs().len(), 1);
        assert_eq!(graph.subgraphs()[0].directive, SubgraphDirective::Loop);

        // Check handler names
        assert_eq!(
            graph.node(&"lp/loop-start".into()).unwrap().handler.as_deref(),
            Some(HANDLER_LOOP_START)
        );
        assert_eq!(
            graph.node(&"lp/loop-end".into()).unwrap().handler.as_deref(),
            Some(HANDLER_LOOP_END)
        );
    }

    #[test]
    fn loop_with_steps_array() {
        let steps = vec![json!({
            "id": "lp",
            "type": "loop",
            "over": "files",
            "steps": [
                {"id": "read", "type": "read-file"},
                {"id": "process", "type": "agent"},
            ]
        })];
        let graph = compile_steps("test", &steps).unwrap();
        // loop-start + read + process + loop-end = 4
        assert_eq!(graph.node_count(), 4);
        // start->read, read->process, process->end = 3
        assert_eq!(graph.edge_count(), 3);
    }

    #[test]
    fn conditional_expansion() {
        let steps = vec![json!({
            "id": "cond",
            "type": "conditional",
            "condition": "x > 0",
            "then": [{"id": "yes", "type": "agent", "prompt": "yes"}],
            "else": [{"id": "no", "type": "agent", "prompt": "no"}]
        })];
        let graph = compile_steps("test", &steps).unwrap();
        // branch + yes + no + merge = 4
        assert_eq!(graph.node_count(), 4);
        assert!(graph.node(&"cond/branch".into()).is_some());
        assert!(graph.node(&"cond/merge".into()).is_some());

        assert_eq!(
            graph.node(&"cond/branch".into()).unwrap().handler.as_deref(),
            Some(HANDLER_BRANCH)
        );
        assert_eq!(
            graph.node(&"cond/merge".into()).unwrap().handler.as_deref(),
            Some(HANDLER_MERGE)
        );
    }

    #[test]
    fn conditional_no_else() {
        let steps = vec![json!({
            "id": "cond",
            "type": "conditional",
            "condition": "ready",
            "then": [{"id": "go", "type": "agent"}]
        })];
        let graph = compile_steps("test", &steps).unwrap();
        // branch + go + merge = 3
        assert_eq!(graph.node_count(), 3);
        // branch->go, go->merge(then), branch->merge(else) = 3
        assert_eq!(graph.edge_count(), 3);
    }

    #[test]
    fn serial_expansion() {
        let steps = vec![json!({
            "id": "seq",
            "type": "serial",
            "steps": [
                {"id": "first", "type": "agent"},
                {"id": "second", "type": "agent"},
            ]
        })];
        let graph = compile_steps("test", &steps).unwrap();
        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 1);
        assert!(graph.node(&"seq/first".into()).is_some());
        assert!(graph.node(&"seq/second".into()).is_some());
    }

    #[test]
    fn nested_parallel_in_serial() {
        let steps = vec![
            json!({"id": "start", "type": "agent"}),
            json!({
                "id": "par",
                "type": "parallel",
                "steps": [
                    {"id": "a", "type": "agent"},
                    {"id": "b", "type": "agent"},
                ]
            }),
            json!({"id": "end", "type": "agent"}),
        ];
        let graph = compile_steps("test", &steps).unwrap();
        // start + fork + a + b + join + end = 6
        assert_eq!(graph.node_count(), 6);
        // start->fork, fork->a, fork->b, a->join, b->join, join->end = 6
        assert_eq!(graph.edge_count(), 6);
    }

    #[test]
    fn nested_loop_in_parallel() {
        let steps = vec![json!({
            "id": "par",
            "type": "parallel",
            "steps": [
                {"id": "simple", "type": "agent"},
                {
                    "id": "lp",
                    "type": "loop",
                    "over": "items",
                    "step": {"id": "body", "type": "agent"}
                }
            ]
        })];
        let graph = compile_steps("test", &steps).unwrap();
        // fork + simple + loop-start + body + loop-end + join = 6
        assert_eq!(graph.node_count(), 6);
        // Subgraphs: 1 parallel + 1 loop = 2
        assert_eq!(graph.subgraphs().len(), 2);
    }

    #[test]
    fn mermaid_round_trip() {
        let steps = vec![
            json!({"id": "a", "type": "agent", "prompt": "go"}),
            json!({
                "id": "par",
                "type": "parallel",
                "steps": [
                    {"id": "b", "type": "agent"},
                    {"id": "c", "type": "agent"},
                ]
            }),
            json!({"id": "d", "type": "agent"}),
        ];
        let graph = compile_steps("test", &steps).unwrap();
        let mermaid = export_mermaid(&graph);
        assert!(!mermaid.is_empty());
        assert!(mermaid.contains("a"), "mermaid should contain node 'a'");
        assert!(mermaid.contains("par/fork"), "mermaid should contain 'par/fork'");
        assert!(mermaid.contains("par/join"), "mermaid should contain 'par/join'");
    }

    #[test]
    fn is_structural_type_check() {
        assert!(is_structural_type("parallel"));
        assert!(is_structural_type("serial"));
        assert!(is_structural_type("loop"));
        assert!(is_structural_type("conditional"));
        assert!(!is_structural_type("agent"));
        assert!(!is_structural_type("custom-thing"));
    }

    #[test]
    fn node_config_contains_full_step() {
        let steps = vec![json!({
            "id": "x",
            "type": "agent",
            "agent": "Athena",
            "prompt": "do stuff",
            "model": "opus"
        })];
        let graph = compile_steps("test", &steps).unwrap();
        let node = graph.node(&"x".into()).unwrap();
        assert_eq!(node.config["agent"], "Athena");
        assert_eq!(node.config["prompt"], "do stuff");
        assert_eq!(node.config["model"], "opus");
    }

    #[test]
    fn graph_metadata_name() {
        let graph = compile_steps("my-workflow", &[]).unwrap();
        assert_eq!(graph.metadata().name.as_deref(), Some("my-workflow"));
    }

    #[test]
    fn deeply_nested() {
        let steps = vec![json!({
            "id": "outer",
            "type": "serial",
            "steps": [{
                "id": "mid",
                "type": "parallel",
                "steps": [
                    {
                        "id": "inner-loop",
                        "type": "loop",
                        "over": "things",
                        "step": {"id": "leaf", "type": "agent"}
                    },
                    {"id": "simple", "type": "agent"}
                ]
            }]
        })];
        let graph = compile_steps("test", &steps).unwrap();
        // fork + loop-start + leaf + loop-end + simple + join = 6
        assert_eq!(graph.node_count(), 6);
        // 1 parallel subgraph + 1 loop subgraph
        assert_eq!(graph.subgraphs().len(), 2);

        // Verify namespaced IDs
        assert!(graph.node(&"outer/mid/fork".into()).is_some());
        assert!(graph.node(&"outer/mid/inner-loop/loop-start".into()).is_some());
        assert!(graph.node(&"outer/mid/inner-loop/leaf".into()).is_some());
        assert!(graph.node(&"outer/mid/inner-loop/loop-end".into()).is_some());
        assert!(graph.node(&"outer/mid/simple".into()).is_some());
        assert!(graph.node(&"outer/mid/join".into()).is_some());
    }

    // -- Edge cases from review --

    #[test]
    fn duplicate_step_ids_error() {
        let steps = vec![
            json!({"id": "dup", "type": "agent"}),
            json!({"id": "dup", "type": "agent"}),
        ];
        let err = compile_steps("test", &steps).unwrap_err();
        assert!(err.contains("dup"), "error should mention the duplicate id");
    }

    #[test]
    fn step_id_with_slash_rejected() {
        let steps = vec![json!({"id": "a/b", "type": "agent"})];
        let err = compile_steps("test", &steps).unwrap_err();
        assert!(err.contains("must not contain '/'"));
    }

    #[test]
    fn empty_parallel_steps() {
        let steps = vec![json!({
            "id": "par",
            "type": "parallel",
            "steps": []
        })];
        let graph = compile_steps("test", &steps).unwrap();
        // fork + join only, no branches
        assert_eq!(graph.node_count(), 2);
        assert_eq!(graph.edge_count(), 0);
        assert!(graph.node(&"par/fork".into()).is_some());
        assert!(graph.node(&"par/join".into()).is_some());
    }

    #[test]
    fn empty_serial_steps() {
        let steps = vec![
            json!({"id": "before", "type": "agent"}),
            json!({"id": "seq", "type": "serial", "steps": []}),
            json!({"id": "after", "type": "agent"}),
        ];
        let graph = compile_steps("test", &steps).unwrap();
        // before + after (serial produces no nodes)
        assert_eq!(graph.node_count(), 2);
        // before->after (serial returns prev=before, so after chains from it)
        assert_eq!(graph.edge_count(), 1);
    }

    #[test]
    fn conditional_only_else() {
        let steps = vec![json!({
            "id": "cond",
            "type": "conditional",
            "condition": "check",
            "else": [{"id": "fallback", "type": "agent"}]
        })];
        let graph = compile_steps("test", &steps).unwrap();
        // branch + fallback + merge = 3
        assert_eq!(graph.node_count(), 3);
        // branch->fallback, branch->merge(then), fallback->merge(else) = 3
        assert_eq!(graph.edge_count(), 3);
    }

    #[test]
    fn loop_no_body() {
        let steps = vec![json!({
            "id": "lp",
            "type": "loop",
            "over": "items"
        })];
        let graph = compile_steps("test", &steps).unwrap();
        // loop-start + loop-end (no body nodes)
        assert_eq!(graph.node_count(), 2);
        // start->end
        assert_eq!(graph.edge_count(), 1);
        assert!(graph.subgraphs()[0].nodes.is_empty());
    }

    #[test]
    fn join_receives_named_ports() {
        let steps = vec![json!({
            "id": "par",
            "type": "parallel",
            "steps": [
                {"id": "alpha", "type": "agent"},
                {"id": "beta", "type": "agent"},
            ]
        })];
        let graph = compile_steps("test", &steps).unwrap();
        let join_edges = graph.incoming_edges(&"par/join".into());
        let port_names: Vec<&str> = join_edges.iter().map(|(_, e)| e.target_port.as_str()).collect();
        assert!(port_names.contains(&"alpha"));
        assert!(port_names.contains(&"beta"));
    }

    #[test]
    fn merge_receives_named_ports() {
        let steps = vec![json!({
            "id": "cond",
            "type": "conditional",
            "condition": "x",
            "then": [{"id": "t", "type": "agent"}],
            "else": [{"id": "e", "type": "agent"}]
        })];
        let graph = compile_steps("test", &steps).unwrap();
        let merge_edges = graph.incoming_edges(&"cond/merge".into());
        let port_names: Vec<&str> = merge_edges.iter().map(|(_, e)| e.target_port.as_str()).collect();
        assert!(port_names.contains(&"then"));
        assert!(port_names.contains(&"else"));
    }
}
