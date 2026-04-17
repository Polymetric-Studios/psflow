//! Opinionated helpers over [`Blackboard`] for common workflow conventions.
//!
//! These routines implement patterns originally developed in ergon-core and
//! upstreamed for reuse. They cover:
//!
//! - **Workflow state layout.** `init` writes `workflow_inputs`, `workflow_constants`,
//!   and an empty `workflow_results` map at well-known global keys. `build_context_maps`
//!   reads them back.
//! - **Per-step result aggregation.** `set_result` applies a [`ResultReducer`]
//!   to combine a new value with any existing entry, and — when the reducer is
//!   [`ResultReducer::Promote`] — also writes the value under a name-addressable
//!   key so downstream nodes can fetch by step id.
//! - **Loop variables.** `push_loop_vars`/`pop_loop_vars`/`update_loop_vars`
//!   manage a stack of [`LoopVars`] so nested loops track item/index/total
//!   without colliding.
//! - **Break signal.** `has_break_signal`/`clear_break_signal` coordinate
//!   loop-control nodes with the loop iterator.
//! - **Output directory.** `set_output_dir` records a per-run output path.
//! - **Raw map helpers.** `read_map`/`write_map` bridge a
//!   `BTreeMap<String, serde_json::Value>` and a [`Value::Map`].
//!
//! All helpers write under [`BlackboardScope::Global`].

use crate::blackboard::{Blackboard, BlackboardScope};
use crate::graph::types::{ResultReducer, Value};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Well-known keys
// ---------------------------------------------------------------------------

/// Blackboard key holding the workflow's input map.
pub const WORKFLOW_INPUTS: &str = "wf:inputs";
/// Blackboard key holding the workflow's constants map.
pub const WORKFLOW_CONSTANTS: &str = "wf:constants";
/// Blackboard key holding the per-step results map (keyed by step id).
pub const WORKFLOW_RESULTS: &str = "wf:results";
/// Blackboard key holding the stack of [`LoopVars`] for nested loops.
pub const WORKFLOW_LOOP_STACK: &str = "wf:loop_stack";
/// Blackboard key holding the current run's output directory path (string).
pub const WORKFLOW_OUTPUT_DIR: &str = "wf:output_dir";
/// Blackboard key prefix for values promoted to name-addressable lookup.
pub const PROMOTED_PREFIX: &str = "wf:promoted:";
/// Blackboard key holding the loop break signal. When `true`, the current
/// loop iteration completes but no further iterations start.
pub const LOOP_BREAK: &str = "wf:loop_break";

// ---------------------------------------------------------------------------
// Loop variables
// ---------------------------------------------------------------------------

/// Per-iteration loop state pushed on the workflow loop stack.
///
/// `item` is the current element, `index` is the zero-based position, and
/// `total` is the total count if known. Inner loops shadow outer loops — the
/// top of the stack is the active iteration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopVars {
    pub item: serde_json::Value,
    pub index: usize,
    pub total: Option<usize>,
}

// ---------------------------------------------------------------------------
// Raw map read/write
// ---------------------------------------------------------------------------

/// Write a `BTreeMap<String, serde_json::Value>` into the blackboard at `key`
/// under [`BlackboardScope::Global`] as a [`Value::Map`].
pub fn write_map(bb: &mut Blackboard, key: &str, map: &BTreeMap<String, serde_json::Value>) {
    let value_map: BTreeMap<String, Value> = map
        .iter()
        .map(|(k, v)| (k.clone(), Value::from(v.clone())))
        .collect();
    bb.set(
        key.to_owned(),
        Value::Map(value_map),
        BlackboardScope::Global,
    );
}

/// Read a `BTreeMap<String, serde_json::Value>` from the blackboard at `key`.
///
/// Returns an empty map if the key is absent or holds a non-map value.
pub fn read_map(bb: &Blackboard, key: &str) -> BTreeMap<String, serde_json::Value> {
    match bb.get(key, &BlackboardScope::Global) {
        Some(Value::Map(m)) => m
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::from(v)))
            .collect(),
        _ => BTreeMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Workflow-state layout
// ---------------------------------------------------------------------------

/// Seed the blackboard with workflow inputs, constants, empty results, and an
/// empty loop stack.
///
/// Idempotent — re-calling overwrites `WORKFLOW_INPUTS`, `WORKFLOW_CONSTANTS`,
/// `WORKFLOW_RESULTS`, and `WORKFLOW_LOOP_STACK` with the provided values.
pub fn init(
    bb: &mut Blackboard,
    inputs: &BTreeMap<String, serde_json::Value>,
    constants: &BTreeMap<String, serde_json::Value>,
) {
    write_map(bb, WORKFLOW_INPUTS, inputs);
    write_map(bb, WORKFLOW_CONSTANTS, constants);
    write_map(bb, WORKFLOW_RESULTS, &BTreeMap::new());
    bb.set(
        WORKFLOW_LOOP_STACK.to_owned(),
        Value::Vec(vec![]),
        BlackboardScope::Global,
    );
}

/// Read the inputs / results / constants maps back in a single bundle.
///
/// Also returns the active loop variables (top of the loop stack, if any) and
/// the output directory path (if set). Embedders typically combine this with
/// their own template-context shape.
pub struct WorkflowStateView {
    pub inputs: BTreeMap<String, serde_json::Value>,
    pub results: BTreeMap<String, serde_json::Value>,
    pub constants: BTreeMap<String, serde_json::Value>,
    pub loop_vars: Option<LoopVars>,
    pub output_dir: Option<String>,
}

/// Read the current workflow-state view from the blackboard.
pub fn build_context_maps(bb: &Blackboard) -> WorkflowStateView {
    let loop_vars = read_loop_stack(bb).last().and_then(value_to_loop_vars);

    let output_dir = bb
        .get(WORKFLOW_OUTPUT_DIR, &BlackboardScope::Global)
        .and_then(|v| match v {
            Value::String(s) => Some(s.clone()),
            _ => None,
        });

    WorkflowStateView {
        inputs: read_map(bb, WORKFLOW_INPUTS),
        results: read_map(bb, WORKFLOW_RESULTS),
        constants: read_map(bb, WORKFLOW_CONSTANTS),
        loop_vars,
        output_dir,
    }
}

/// Record the output directory path for the current run.
pub fn set_output_dir(bb: &mut Blackboard, path: &str) {
    bb.set(
        WORKFLOW_OUTPUT_DIR.to_owned(),
        Value::String(path.to_owned()),
        BlackboardScope::Global,
    );
}

// ---------------------------------------------------------------------------
// Per-step result aggregation
// ---------------------------------------------------------------------------

/// Write a single step's result into `WORKFLOW_RESULTS`, applying `reducer` to
/// combine it with any existing entry.
///
/// When `reducer` is [`ResultReducer::Promote`], the value is additionally
/// written to `PROMOTED_PREFIX + step_id` so downstream consumers can fetch
/// the value by name via [`get_value`].
pub fn set_result(
    bb: &mut Blackboard,
    step_id: &str,
    value: serde_json::Value,
    reducer: &ResultReducer,
) {
    let mut results = read_map(bb, WORKFLOW_RESULTS);
    let existing = results.get(step_id);
    let merged = reducer.apply(existing, value.clone());
    results.insert(step_id.to_owned(), merged);
    write_map(bb, WORKFLOW_RESULTS, &results);

    if matches!(reducer, ResultReducer::Promote) {
        let promoted_key = format!("{PROMOTED_PREFIX}{step_id}");
        bb.set(promoted_key, Value::from(value), BlackboardScope::Global);
    }
}

/// List all promoted keys currently on the blackboard (those starting with
/// [`PROMOTED_PREFIX`]).
pub fn promoted_keys(bb: &Blackboard) -> Vec<String> {
    bb.global()
        .keys()
        .filter(|k| k.starts_with(PROMOTED_PREFIX))
        .cloned()
        .collect()
}

/// Fetch a value from the global scope by key, returning its JSON form.
pub fn get_value(bb: &Blackboard, key: &str) -> Option<serde_json::Value> {
    bb.get(key, &BlackboardScope::Global)
        .map(serde_json::Value::from)
}

// ---------------------------------------------------------------------------
// Loop stack
// ---------------------------------------------------------------------------

/// Push `vars` onto the loop stack.
pub fn push_loop_vars(bb: &mut Blackboard, vars: &LoopVars) {
    let mut stack = read_loop_stack(bb);
    stack.push(loop_vars_to_value(vars));
    bb.set(
        WORKFLOW_LOOP_STACK.to_owned(),
        Value::Vec(stack),
        BlackboardScope::Global,
    );
}

/// Pop the top entry off the loop stack (no-op if empty).
pub fn pop_loop_vars(bb: &mut Blackboard) {
    let mut stack = read_loop_stack(bb);
    stack.pop();
    bb.set(
        WORKFLOW_LOOP_STACK.to_owned(),
        Value::Vec(stack),
        BlackboardScope::Global,
    );
}

/// Replace the top loop-stack entry with new iteration values.
///
/// Useful for step-through iterators that advance without push/pop churn.
pub fn update_loop_vars(bb: &mut Blackboard, item: serde_json::Value, index: usize, total: usize) {
    let mut stack = read_loop_stack(bb);
    if let Some(top) = stack.last_mut() {
        *top = loop_vars_to_value(&LoopVars {
            item,
            index,
            total: Some(total),
        });
    }
    bb.set(
        WORKFLOW_LOOP_STACK.to_owned(),
        Value::Vec(stack),
        BlackboardScope::Global,
    );
}

// ---------------------------------------------------------------------------
// Break signal
// ---------------------------------------------------------------------------

/// Check whether a loop-break signal has been set.
pub fn has_break_signal(bb: &Blackboard) -> bool {
    matches!(
        bb.get(LOOP_BREAK, &BlackboardScope::Global),
        Some(Value::Bool(true))
    )
}

/// Clear the loop-break signal.
pub fn clear_break_signal(bb: &mut Blackboard) {
    bb.set(
        LOOP_BREAK.to_owned(),
        Value::Bool(false),
        BlackboardScope::Global,
    );
}

/// Raise the loop-break signal. Loop iterators read this via
/// [`has_break_signal`] to decide whether to start another iteration.
pub fn set_break_signal(bb: &mut Blackboard) {
    bb.set(
        LOOP_BREAK.to_owned(),
        Value::Bool(true),
        BlackboardScope::Global,
    );
}

// ---------------------------------------------------------------------------
// Internal conversions
// ---------------------------------------------------------------------------

fn read_loop_stack(bb: &Blackboard) -> Vec<Value> {
    match bb.get(WORKFLOW_LOOP_STACK, &BlackboardScope::Global) {
        Some(Value::Vec(v)) => v.clone(),
        _ => vec![],
    }
}

fn loop_vars_to_value(vars: &LoopVars) -> Value {
    let mut m = BTreeMap::new();
    m.insert("item".to_owned(), Value::from(vars.item.clone()));
    m.insert("index".to_owned(), Value::I64(vars.index as i64));
    if let Some(total) = vars.total {
        m.insert("total".to_owned(), Value::I64(total as i64));
    }
    Value::Map(m)
}

fn value_to_loop_vars(v: &Value) -> Option<LoopVars> {
    if let Value::Map(m) = v {
        let item = m
            .get("item")
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null);
        let index = match m.get("index") {
            Some(Value::I64(n)) => *n as usize,
            _ => 0,
        };
        let total = match m.get("total") {
            Some(Value::I64(n)) => Some(*n as usize),
            _ => None,
        };
        Some(LoopVars { item, index, total })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_bb() -> Blackboard {
        Blackboard::new()
    }

    #[test]
    fn init_writes_empty_defaults() {
        let mut bb = fresh_bb();
        init(&mut bb, &BTreeMap::new(), &BTreeMap::new());
        assert!(read_map(&bb, WORKFLOW_INPUTS).is_empty());
        assert!(read_map(&bb, WORKFLOW_RESULTS).is_empty());
        assert!(read_map(&bb, WORKFLOW_CONSTANTS).is_empty());
    }

    #[test]
    fn init_preserves_inputs_and_constants() {
        let mut bb = fresh_bb();
        let mut inputs = BTreeMap::new();
        inputs.insert("name".into(), serde_json::json!("World"));
        let mut constants = BTreeMap::new();
        constants.insert("count".into(), serde_json::json!(42));
        init(&mut bb, &inputs, &constants);
        assert_eq!(
            read_map(&bb, WORKFLOW_INPUTS).get("name"),
            Some(&serde_json::json!("World"))
        );
        assert_eq!(
            read_map(&bb, WORKFLOW_CONSTANTS).get("count"),
            Some(&serde_json::json!(42))
        );
    }

    #[test]
    fn set_result_replace_stores_in_results_map() {
        let mut bb = fresh_bb();
        init(&mut bb, &BTreeMap::new(), &BTreeMap::new());
        set_result(
            &mut bb,
            "step_a",
            serde_json::json!("hello"),
            &ResultReducer::Replace,
        );
        let results = read_map(&bb, WORKFLOW_RESULTS);
        assert_eq!(results.get("step_a"), Some(&serde_json::json!("hello")));
    }

    #[test]
    fn set_result_promote_also_writes_named_key() {
        let mut bb = fresh_bb();
        init(&mut bb, &BTreeMap::new(), &BTreeMap::new());
        set_result(
            &mut bb,
            "triage",
            serde_json::json!("output"),
            &ResultReducer::Promote,
        );

        let results = read_map(&bb, WORKFLOW_RESULTS);
        assert_eq!(results.get("triage"), Some(&serde_json::json!("output")));

        let promoted = get_value(&bb, "wf:promoted:triage");
        assert_eq!(promoted, Some(serde_json::json!("output")));
    }

    #[test]
    fn set_result_replace_does_not_promote() {
        let mut bb = fresh_bb();
        init(&mut bb, &BTreeMap::new(), &BTreeMap::new());
        set_result(
            &mut bb,
            "step",
            serde_json::json!("x"),
            &ResultReducer::Replace,
        );
        assert!(get_value(&bb, "wf:promoted:step").is_none());
    }

    #[test]
    fn promoted_keys_lists_only_promoted() {
        let mut bb = fresh_bb();
        init(&mut bb, &BTreeMap::new(), &BTreeMap::new());
        set_result(
            &mut bb,
            "a",
            serde_json::json!("x"),
            &ResultReducer::Promote,
        );
        set_result(
            &mut bb,
            "b",
            serde_json::json!("y"),
            &ResultReducer::Promote,
        );
        set_result(
            &mut bb,
            "c",
            serde_json::json!("z"),
            &ResultReducer::Replace,
        );
        let mut keys = promoted_keys(&bb);
        keys.sort();
        assert_eq!(keys, vec!["wf:promoted:a", "wf:promoted:b"]);
    }

    #[test]
    fn loop_stack_push_pop_tracks_top() {
        let mut bb = fresh_bb();
        init(&mut bb, &BTreeMap::new(), &BTreeMap::new());

        push_loop_vars(
            &mut bb,
            &LoopVars {
                item: serde_json::json!("A"),
                index: 0,
                total: Some(2),
            },
        );
        push_loop_vars(
            &mut bb,
            &LoopVars {
                item: serde_json::json!("B"),
                index: 1,
                total: Some(3),
            },
        );

        let view = build_context_maps(&bb);
        assert_eq!(
            view.loop_vars.as_ref().unwrap().item,
            serde_json::json!("B")
        );

        pop_loop_vars(&mut bb);
        let view = build_context_maps(&bb);
        assert_eq!(
            view.loop_vars.as_ref().unwrap().item,
            serde_json::json!("A")
        );

        pop_loop_vars(&mut bb);
        let view = build_context_maps(&bb);
        assert!(view.loop_vars.is_none());
    }

    #[test]
    fn update_loop_vars_replaces_top() {
        let mut bb = fresh_bb();
        init(&mut bb, &BTreeMap::new(), &BTreeMap::new());
        push_loop_vars(
            &mut bb,
            &LoopVars {
                item: serde_json::json!("first"),
                index: 0,
                total: Some(3),
            },
        );
        update_loop_vars(&mut bb, serde_json::json!("second"), 1, 3);
        let view = build_context_maps(&bb);
        assert_eq!(
            view.loop_vars.as_ref().unwrap().item,
            serde_json::json!("second")
        );
        assert_eq!(view.loop_vars.as_ref().unwrap().index, 1);
    }

    #[test]
    fn break_signal_round_trip() {
        let mut bb = fresh_bb();
        assert!(!has_break_signal(&bb));
        set_break_signal(&mut bb);
        assert!(has_break_signal(&bb));
        clear_break_signal(&mut bb);
        assert!(!has_break_signal(&bb));
    }

    #[test]
    fn output_dir_round_trip() {
        let mut bb = fresh_bb();
        init(&mut bb, &BTreeMap::new(), &BTreeMap::new());
        set_output_dir(&mut bb, "/tmp/runs/abc");
        let view = build_context_maps(&bb);
        assert_eq!(view.output_dir.as_deref(), Some("/tmp/runs/abc"));
    }

    #[test]
    fn get_value_missing_returns_none() {
        let bb = fresh_bb();
        assert!(get_value(&bb, "wf:promoted:nonexistent").is_none());
    }
}
