//! Default handlers for step compiler control nodes.
//!
//! These provide sensible generic behavior for the structural nodes emitted by
//! [`compile_steps`](crate::graph::step_compiler::compile_steps). Callers can
//! override any of them by registering their own handler under the same name.

use crate::error::NodeError;
use crate::execute::{CancellationToken, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::graph::types::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

// ---------------------------------------------------------------------------
// StepForkHandler — passthrough (distributes inputs to parallel branches)
// ---------------------------------------------------------------------------

/// Passes all inputs through unchanged. The graph topology handles fan-out.
pub struct StepForkHandler;

impl NodeHandler for StepForkHandler {
    fn execute(
        &self,
        _node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        Box::pin(async move { Ok(inputs) })
    }
}

// ---------------------------------------------------------------------------
// StepJoinHandler — collects named-port inputs into a nested map
// ---------------------------------------------------------------------------

/// Collects all named-port inputs into a `Map { port_name: value }` output.
///
/// Used for both parallel join (`step:join`) and conditional merge (`step:merge`).
/// For parallel: each branch result arrives on a port named after the branch's
/// step ID. For conditional: results arrive on "then" or "else" ports.
///
/// The "in" port (default edge port) is excluded from the collected map but
/// passed through if it's the only input.
///
/// **Merge semantic note:** For conditionals, only one branch typically executes
/// (the other is cancelled), so this produces a single-entry map like
/// `{"then": value}`. Callers that need a bare value (not wrapped in a map)
/// should override `step:merge` with a handler that unwraps the active branch.
pub struct StepJoinHandler;

impl NodeHandler for StepJoinHandler {
    fn execute(
        &self,
        _node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        Box::pin(async move {
            let mut collected = BTreeMap::new();

            for (key, val) in &inputs {
                if key != "in" {
                    collected.insert(key.clone(), val.clone());
                }
            }

            let mut outputs = Outputs::new();
            if collected.is_empty() {
                // No named ports — pass through the default "in" input
                if let Some(val) = inputs.get("in") {
                    outputs.insert("out".into(), val.clone());
                }
            } else {
                outputs.insert("out".into(), Value::Map(collected));
            }
            Ok(outputs)
        })
    }
}

// ---------------------------------------------------------------------------
// StepBranchHandler — passthrough (condition evaluation is caller's concern)
// ---------------------------------------------------------------------------

/// Passes all inputs through unchanged. Callers override this to evaluate
/// conditions and control which branch executes.
///
/// **Note:** The default passthrough means both then/else branches will execute.
/// To implement actual conditional logic, override `step:branch` with a handler
/// that sets a `branch_decision` on the execution context, or use the executor's
/// guard mechanism (the step compiler stores the condition in `node.config["condition"]`).
pub struct StepBranchHandler;

impl NodeHandler for StepBranchHandler {
    fn execute(
        &self,
        _node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        Box::pin(async move { Ok(inputs) })
    }
}

// ---------------------------------------------------------------------------
// StepLoopStartHandler — passthrough
// ---------------------------------------------------------------------------

/// Passes all inputs through unchanged. The LoopController manages iteration
/// state externally.
pub struct StepLoopStartHandler;

impl NodeHandler for StepLoopStartHandler {
    fn execute(
        &self,
        _node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        Box::pin(async move { Ok(inputs) })
    }
}

// ---------------------------------------------------------------------------
// StepLoopEndHandler — passthrough hook point
// ---------------------------------------------------------------------------

/// Passes all inputs through unchanged. Callers can override this to
/// accumulate loop iteration results or perform post-iteration logic.
pub struct StepLoopEndHandler;

impl NodeHandler for StepLoopEndHandler {
    fn execute(
        &self,
        _node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        Box::pin(async move { Ok(inputs) })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::node::Node;
    use tokio_util::sync::CancellationToken as TokioCancellation;

    fn cancel() -> CancellationToken {
        TokioCancellation::new()
    }

    fn node(id: &str) -> Node {
        Node::new(id, id)
    }

    // -- StepForkHandler --

    #[tokio::test]
    async fn fork_passes_through() {
        let inputs: Outputs = [("in".into(), Value::String("data".into()))].into();
        let result = StepForkHandler
            .execute(&node("fork"), inputs.clone(), cancel())
            .await
            .unwrap();
        assert_eq!(result, inputs);
    }

    // -- StepJoinHandler --

    #[tokio::test]
    async fn join_collects_named_ports() {
        let inputs: Outputs = [
            ("alpha".into(), Value::String("result-a".into())),
            ("beta".into(), Value::String("result-b".into())),
        ]
        .into();
        let result = StepJoinHandler
            .execute(&node("join"), inputs, cancel())
            .await
            .unwrap();

        let out = result.get("out").expect("should have 'out' key");
        match out {
            Value::Map(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(map["alpha"], Value::String("result-a".into()));
                assert_eq!(map["beta"], Value::String("result-b".into()));
            }
            _ => panic!("expected Map, got {out:?}"),
        }
    }

    #[tokio::test]
    async fn join_excludes_default_in_port() {
        let inputs: Outputs = [
            ("in".into(), Value::String("ignored".into())),
            ("branch1".into(), Value::String("kept".into())),
        ]
        .into();
        let result = StepJoinHandler
            .execute(&node("join"), inputs, cancel())
            .await
            .unwrap();

        let out = result.get("out").expect("should have 'out' key");
        match out {
            Value::Map(map) => {
                assert_eq!(map.len(), 1);
                assert!(!map.contains_key("in"));
                assert_eq!(map["branch1"], Value::String("kept".into()));
            }
            _ => panic!("expected Map, got {out:?}"),
        }
    }

    #[tokio::test]
    async fn join_passes_through_when_only_in() {
        let inputs: Outputs = [("in".into(), Value::String("solo".into()))].into();
        let result = StepJoinHandler
            .execute(&node("join"), inputs, cancel())
            .await
            .unwrap();

        let out = result.get("out").expect("should have 'out' key");
        assert_eq!(out, &Value::String("solo".into()));
    }

    #[tokio::test]
    async fn join_empty_inputs() {
        let inputs: Outputs = Outputs::new();
        let result = StepJoinHandler
            .execute(&node("join"), inputs, cancel())
            .await
            .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn join_single_named_port_conditional_case() {
        // Simulates conditional merge where only one branch ran
        let inputs: Outputs = [("then".into(), Value::String("yes-result".into()))].into();
        let result = StepJoinHandler
            .execute(&node("merge"), inputs, cancel())
            .await
            .unwrap();

        let out = result.get("out").expect("should have 'out' key");
        match out {
            Value::Map(map) => {
                assert_eq!(map.len(), 1);
                assert_eq!(map["then"], Value::String("yes-result".into()));
            }
            _ => panic!("expected Map, got {out:?}"),
        }
    }

    #[tokio::test]
    async fn join_with_null_branch() {
        // Branch that produced null output alongside a real result
        let inputs: Outputs = [
            ("alpha".into(), Value::String("real".into())),
            ("beta".into(), Value::Null),
        ]
        .into();
        let result = StepJoinHandler
            .execute(&node("join"), inputs, cancel())
            .await
            .unwrap();

        let out = result.get("out").expect("should have 'out' key");
        match out {
            Value::Map(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(map["alpha"], Value::String("real".into()));
                assert_eq!(map["beta"], Value::Null);
            }
            _ => panic!("expected Map, got {out:?}"),
        }
    }

    // -- StepBranchHandler --

    #[tokio::test]
    async fn branch_passes_through() {
        let inputs: Outputs = [("in".into(), Value::Bool(true))].into();
        let result = StepBranchHandler
            .execute(&node("branch"), inputs.clone(), cancel())
            .await
            .unwrap();
        assert_eq!(result, inputs);
    }

    // -- StepLoopStartHandler --

    #[tokio::test]
    async fn loop_start_passes_through() {
        let inputs: Outputs = [("in".into(), Value::I64(42))].into();
        let result = StepLoopStartHandler
            .execute(&node("loop-start"), inputs.clone(), cancel())
            .await
            .unwrap();
        assert_eq!(result, inputs);
    }

    // -- StepLoopEndHandler --

    #[tokio::test]
    async fn loop_end_passes_through() {
        let inputs: Outputs = [("in".into(), Value::String("iteration-result".into()))].into();
        let result = StepLoopEndHandler
            .execute(&node("loop-end"), inputs.clone(), cancel())
            .await
            .unwrap();
        assert_eq!(result, inputs);
    }
}
