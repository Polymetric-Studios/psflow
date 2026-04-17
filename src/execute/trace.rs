//! Execution trace — structured record of a graph execution for replay and debugging.
//!
//! Built from raw `ExecutionEvent`s, the trace provides an ordered, queryable
//! record of which nodes ran, in what order, with what data, and how long each took.

use crate::error::NodeError;
use crate::execute::event::ExecutionEvent;
use crate::execute::lifecycle::NodeState;
use crate::execute::Outputs;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// A complete execution trace, built from raw events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionTrace {
    /// Per-node records in execution completion order.
    pub records: Vec<TraceRecord>,
    /// Total execution time.
    pub elapsed: Duration,
}

/// A single node's execution record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    /// Node identifier.
    pub node_id: String,
    /// Execution order (0-based, by completion time).
    pub order: usize,
    /// Final state of the node.
    pub state: NodeState,
    /// Time from when the node started running to completion (if available).
    pub elapsed: Option<Duration>,
    /// Output values produced by the node (empty if failed/cancelled).
    pub outputs: Option<Outputs>,
    /// Error if the node failed.
    pub error: Option<NodeError>,
    /// Retry attempts before final result (empty if no retries).
    pub retries: Vec<RetryRecord>,
}

/// Record of a single retry attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryRecord {
    pub attempt: u32,
    pub max_attempts: u32,
    pub error: NodeError,
    pub next_delay_ms: u64,
}

impl ExecutionTrace {
    /// Build a trace from raw execution events.
    ///
    /// Processes events in order to reconstruct per-node timing, outputs,
    /// errors, and retry history.
    pub fn from_events(events: &[ExecutionEvent]) -> Self {
        let mut node_running_since: HashMap<String, std::time::Instant> = HashMap::new();
        let mut node_outputs: HashMap<String, Outputs> = HashMap::new();
        let mut node_errors: HashMap<String, NodeError> = HashMap::new();
        let mut node_retries: HashMap<String, Vec<RetryRecord>> = HashMap::new();
        let mut node_elapsed: HashMap<String, Duration> = HashMap::new();
        let mut node_final_state: HashMap<String, NodeState> = HashMap::new();
        let mut completion_order: Vec<String> = Vec::new();
        let mut total_elapsed = Duration::ZERO;

        for event in events {
            match event {
                ExecutionEvent::StateChanged {
                    node_id,
                    to,
                    timestamp,
                    ..
                } => {
                    if *to == NodeState::Running {
                        node_running_since.insert(node_id.clone(), *timestamp);
                    }
                    if to.is_terminal() {
                        if let Some(start) = node_running_since.get(node_id) {
                            node_elapsed.insert(node_id.clone(), timestamp.duration_since(*start));
                        }
                        node_final_state.insert(node_id.clone(), *to);
                        if !completion_order.contains(node_id) {
                            completion_order.push(node_id.clone());
                        }
                    }
                }
                ExecutionEvent::NodeCompleted { node_id, outputs } => {
                    node_outputs.insert(node_id.clone(), outputs.clone());
                }
                ExecutionEvent::NodeFailed { node_id, error } => {
                    node_errors.insert(node_id.clone(), error.clone());
                }
                ExecutionEvent::NodeRetrying {
                    node_id,
                    attempt,
                    max_attempts,
                    error,
                    next_delay_ms,
                } => {
                    node_retries
                        .entry(node_id.clone())
                        .or_default()
                        .push(RetryRecord {
                            attempt: *attempt,
                            max_attempts: *max_attempts,
                            error: error.clone(),
                            next_delay_ms: *next_delay_ms,
                        });
                }
                ExecutionEvent::ExecutionCompleted { elapsed } => {
                    total_elapsed = *elapsed;
                }
                ExecutionEvent::ExecutionStarted { .. } => {}
            }
        }

        let records: Vec<TraceRecord> = completion_order
            .iter()
            .enumerate()
            .map(|(order, node_id)| TraceRecord {
                node_id: node_id.clone(),
                order,
                state: node_final_state
                    .get(node_id)
                    .copied()
                    .unwrap_or(NodeState::Idle),
                elapsed: node_elapsed.get(node_id).copied(),
                outputs: node_outputs.remove(node_id),
                error: node_errors.remove(node_id),
                retries: node_retries.remove(node_id).unwrap_or_default(),
            })
            .collect();

        ExecutionTrace {
            records,
            elapsed: total_elapsed,
        }
    }

    /// Build a trace filtered to only the ancestors of a given node.
    ///
    /// This gives a node's-eye view of execution history: only nodes on the
    /// paths leading to `node_id` are included. Useful for scoping LLM context
    /// to a node's ancestor chain, excluding parallel branches.
    pub fn for_node(&self, node_id: &str, graph: &crate::graph::Graph) -> Self {
        let ancestors = graph.ancestors(&node_id.into());
        let records: Vec<TraceRecord> = self
            .records
            .iter()
            .filter(|r| ancestors.contains(&r.node_id.as_str().into()))
            .cloned()
            .enumerate()
            .map(|(order, mut r)| {
                r.order = order;
                r
            })
            .collect();
        ExecutionTrace {
            records,
            elapsed: self.elapsed,
        }
    }

    /// Look up a node's trace record by ID.
    pub fn node(&self, node_id: &str) -> Option<&TraceRecord> {
        self.records.iter().find(|r| r.node_id == node_id)
    }

    /// Return node IDs in execution completion order.
    pub fn execution_order(&self) -> Vec<&str> {
        self.records.iter().map(|r| r.node_id.as_str()).collect()
    }

    /// Return records for nodes that failed.
    pub fn failed_nodes(&self) -> Vec<&TraceRecord> {
        self.records
            .iter()
            .filter(|r| r.state == NodeState::Failed)
            .collect()
    }

    /// Return records for nodes that completed successfully.
    pub fn completed_nodes(&self) -> Vec<&TraceRecord> {
        self.records
            .iter()
            .filter(|r| r.state == NodeState::Completed)
            .collect()
    }

    /// Return records for nodes that were cancelled.
    pub fn cancelled_nodes(&self) -> Vec<&TraceRecord> {
        self.records
            .iter()
            .filter(|r| r.state == NodeState::Cancelled)
            .collect()
    }

    /// Return records for nodes that had retry attempts.
    pub fn retried_nodes(&self) -> Vec<&TraceRecord> {
        self.records
            .iter()
            .filter(|r| !r.retries.is_empty())
            .collect()
    }

    /// Serialize the trace to pretty-printed JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::Value;
    use std::time::Instant;

    fn make_events_linear_chain() -> Vec<ExecutionEvent> {
        // Simulate A → B → C execution
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(10);
        let t2 = t0 + Duration::from_millis(20);
        let t3 = t0 + Duration::from_millis(30);
        let t4 = t0 + Duration::from_millis(40);
        let t5 = t0 + Duration::from_millis(50);
        let t6 = t0 + Duration::from_millis(60);
        let t7 = t0 + Duration::from_millis(70);

        vec![
            ExecutionEvent::ExecutionStarted { timestamp: t0 },
            // Node A
            ExecutionEvent::StateChanged {
                node_id: "A".into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: t0,
            },
            ExecutionEvent::StateChanged {
                node_id: "A".into(),
                from: NodeState::Pending,
                to: NodeState::Running,
                timestamp: t1,
            },
            ExecutionEvent::NodeCompleted {
                node_id: "A".into(),
                outputs: {
                    let mut o = Outputs::new();
                    o.insert("result".into(), Value::String("hello".into()));
                    o
                },
            },
            ExecutionEvent::StateChanged {
                node_id: "A".into(),
                from: NodeState::Running,
                to: NodeState::Completed,
                timestamp: t2,
            },
            // Node B
            ExecutionEvent::StateChanged {
                node_id: "B".into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: t2,
            },
            ExecutionEvent::StateChanged {
                node_id: "B".into(),
                from: NodeState::Pending,
                to: NodeState::Running,
                timestamp: t3,
            },
            ExecutionEvent::NodeCompleted {
                node_id: "B".into(),
                outputs: {
                    let mut o = Outputs::new();
                    o.insert("count".into(), Value::I64(42));
                    o
                },
            },
            ExecutionEvent::StateChanged {
                node_id: "B".into(),
                from: NodeState::Running,
                to: NodeState::Completed,
                timestamp: t4,
            },
            // Node C
            ExecutionEvent::StateChanged {
                node_id: "C".into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: t4,
            },
            ExecutionEvent::StateChanged {
                node_id: "C".into(),
                from: NodeState::Pending,
                to: NodeState::Running,
                timestamp: t5,
            },
            ExecutionEvent::NodeCompleted {
                node_id: "C".into(),
                outputs: Outputs::new(),
            },
            ExecutionEvent::StateChanged {
                node_id: "C".into(),
                from: NodeState::Running,
                to: NodeState::Completed,
                timestamp: t6,
            },
            ExecutionEvent::ExecutionCompleted {
                elapsed: t7.duration_since(t0),
            },
        ]
    }

    #[test]
    fn trace_records_correct_order() {
        let events = make_events_linear_chain();
        let trace = ExecutionTrace::from_events(&events);

        assert_eq!(trace.execution_order(), vec!["A", "B", "C"]);
        assert_eq!(trace.records.len(), 3);
    }

    #[test]
    fn trace_records_have_timing() {
        let events = make_events_linear_chain();
        let trace = ExecutionTrace::from_events(&events);

        for record in &trace.records {
            assert!(
                record.elapsed.is_some(),
                "node {} missing elapsed",
                record.node_id
            );
            assert!(record.elapsed.unwrap() > Duration::ZERO);
        }
    }

    #[test]
    fn trace_records_have_outputs() {
        let events = make_events_linear_chain();
        let trace = ExecutionTrace::from_events(&events);

        let a = trace.node("A").unwrap();
        assert_eq!(
            a.outputs.as_ref().unwrap().get("result"),
            Some(&Value::String("hello".into()))
        );

        let b = trace.node("B").unwrap();
        assert_eq!(
            b.outputs.as_ref().unwrap().get("count"),
            Some(&Value::I64(42))
        );
    }

    #[test]
    fn trace_records_have_state() {
        let events = make_events_linear_chain();
        let trace = ExecutionTrace::from_events(&events);

        for record in &trace.records {
            assert_eq!(record.state, NodeState::Completed);
        }
    }

    #[test]
    fn trace_total_elapsed() {
        let events = make_events_linear_chain();
        let trace = ExecutionTrace::from_events(&events);
        assert!(trace.elapsed >= Duration::from_millis(70));
    }

    #[test]
    fn trace_failed_node() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(5);
        let t2 = t0 + Duration::from_millis(10);

        let events = vec![
            ExecutionEvent::ExecutionStarted { timestamp: t0 },
            ExecutionEvent::StateChanged {
                node_id: "X".into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: t0,
            },
            ExecutionEvent::StateChanged {
                node_id: "X".into(),
                from: NodeState::Pending,
                to: NodeState::Running,
                timestamp: t0,
            },
            ExecutionEvent::NodeFailed {
                node_id: "X".into(),
                error: NodeError::Failed {
                    source_message: None,
                    message: "boom".into(),
                    recoverable: false,
                },
            },
            ExecutionEvent::StateChanged {
                node_id: "X".into(),
                from: NodeState::Running,
                to: NodeState::Failed,
                timestamp: t1,
            },
            ExecutionEvent::ExecutionCompleted {
                elapsed: t2.duration_since(t0),
            },
        ];

        let trace = ExecutionTrace::from_events(&events);
        assert_eq!(trace.failed_nodes().len(), 1);
        let x = trace.node("X").unwrap();
        assert_eq!(x.state, NodeState::Failed);
        assert!(x.error.is_some());
        assert!(x.error.as_ref().unwrap().to_string().contains("boom"));
        assert!(x.outputs.is_none());
    }

    #[test]
    fn trace_cancelled_node() {
        let t0 = Instant::now();

        let events = vec![
            ExecutionEvent::ExecutionStarted { timestamp: t0 },
            ExecutionEvent::StateChanged {
                node_id: "Y".into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: t0,
            },
            ExecutionEvent::StateChanged {
                node_id: "Y".into(),
                from: NodeState::Pending,
                to: NodeState::Cancelled,
                timestamp: t0 + Duration::from_millis(5),
            },
            ExecutionEvent::ExecutionCompleted {
                elapsed: Duration::from_millis(5),
            },
        ];

        let trace = ExecutionTrace::from_events(&events);
        assert_eq!(trace.cancelled_nodes().len(), 1);
        assert_eq!(trace.completed_nodes().len(), 0);
    }

    #[test]
    fn trace_retry_records() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(5);
        let t2 = t0 + Duration::from_millis(50);
        let t3 = t0 + Duration::from_millis(60);

        let events = vec![
            ExecutionEvent::ExecutionStarted { timestamp: t0 },
            ExecutionEvent::StateChanged {
                node_id: "R".into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: t0,
            },
            ExecutionEvent::StateChanged {
                node_id: "R".into(),
                from: NodeState::Pending,
                to: NodeState::Running,
                timestamp: t1,
            },
            ExecutionEvent::NodeRetrying {
                node_id: "R".into(),
                attempt: 1,
                max_attempts: 3,
                error: NodeError::Timeout {
                    elapsed_ms: 100,
                    limit_ms: 100,
                },
                next_delay_ms: 500,
            },
            ExecutionEvent::NodeRetrying {
                node_id: "R".into(),
                attempt: 2,
                max_attempts: 3,
                error: NodeError::Timeout {
                    elapsed_ms: 100,
                    limit_ms: 100,
                },
                next_delay_ms: 1000,
            },
            ExecutionEvent::NodeCompleted {
                node_id: "R".into(),
                outputs: Outputs::new(),
            },
            ExecutionEvent::StateChanged {
                node_id: "R".into(),
                from: NodeState::Running,
                to: NodeState::Completed,
                timestamp: t2,
            },
            ExecutionEvent::ExecutionCompleted {
                elapsed: t3.duration_since(t0),
            },
        ];

        let trace = ExecutionTrace::from_events(&events);
        let retried = trace.retried_nodes();
        assert_eq!(retried.len(), 1);

        let r = trace.node("R").unwrap();
        assert_eq!(r.retries.len(), 2);
        assert_eq!(r.retries[0].attempt, 1);
        assert_eq!(r.retries[1].attempt, 2);
        assert_eq!(r.state, NodeState::Completed); // succeeded on attempt 3
    }

    #[test]
    fn trace_empty_events() {
        let trace = ExecutionTrace::from_events(&[]);
        assert!(trace.records.is_empty());
        assert_eq!(trace.elapsed, Duration::ZERO);
    }

    #[test]
    fn trace_node_lookup_missing() {
        let trace = ExecutionTrace::from_events(&[]);
        assert!(trace.node("nonexistent").is_none());
    }

    #[test]
    fn trace_serializes_to_json() {
        let events = make_events_linear_chain();
        let trace = ExecutionTrace::from_events(&events);
        let json = trace.to_json().unwrap();
        assert!(json.contains("\"node_id\": \"A\""));
        assert!(json.contains("\"node_id\": \"B\""));
        assert!(json.contains("\"node_id\": \"C\""));
        assert!(json.contains("\"state\": \"Completed\""));

        // Verify round-trip
        let parsed: ExecutionTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.records.len(), 3);
        assert_eq!(parsed.execution_order(), vec!["A", "B", "C"]);
    }

    #[test]
    fn trace_query_helpers_combined() {
        let events = make_events_linear_chain();
        let trace = ExecutionTrace::from_events(&events);

        assert_eq!(trace.completed_nodes().len(), 3);
        assert_eq!(trace.failed_nodes().len(), 0);
        assert_eq!(trace.cancelled_nodes().len(), 0);
        assert_eq!(trace.retried_nodes().len(), 0);
    }

    // -- Scoped trace tests --

    #[test]
    fn for_node_filters_to_ancestor_path() {
        use crate::graph::node::Node;
        use crate::graph::Graph;

        // Diamond: A → B, A → C, B → D, C → D
        let mut graph = Graph::new();
        graph.add_node(Node::new("A", "A")).unwrap();
        graph.add_node(Node::new("B", "B")).unwrap();
        graph.add_node(Node::new("C", "C")).unwrap();
        graph.add_node(Node::new("D", "D")).unwrap();
        graph
            .add_edge(&"A".into(), "o", &"B".into(), "i", None)
            .unwrap();
        graph
            .add_edge(&"A".into(), "o", &"C".into(), "i", None)
            .unwrap();
        graph
            .add_edge(&"B".into(), "o", &"D".into(), "i", None)
            .unwrap();
        graph
            .add_edge(&"C".into(), "o", &"D".into(), "i", None)
            .unwrap();

        // Simulate all 4 nodes completing
        let t0 = Instant::now();
        let events = vec![
            ExecutionEvent::ExecutionStarted { timestamp: t0 },
            // A completes
            ExecutionEvent::StateChanged {
                node_id: "A".into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: t0,
            },
            ExecutionEvent::StateChanged {
                node_id: "A".into(),
                from: NodeState::Pending,
                to: NodeState::Running,
                timestamp: t0,
            },
            ExecutionEvent::NodeCompleted {
                node_id: "A".into(),
                outputs: Outputs::new(),
            },
            ExecutionEvent::StateChanged {
                node_id: "A".into(),
                from: NodeState::Running,
                to: NodeState::Completed,
                timestamp: t0 + Duration::from_millis(10),
            },
            // B completes
            ExecutionEvent::StateChanged {
                node_id: "B".into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: t0 + Duration::from_millis(10),
            },
            ExecutionEvent::StateChanged {
                node_id: "B".into(),
                from: NodeState::Pending,
                to: NodeState::Running,
                timestamp: t0 + Duration::from_millis(10),
            },
            ExecutionEvent::NodeCompleted {
                node_id: "B".into(),
                outputs: Outputs::new(),
            },
            ExecutionEvent::StateChanged {
                node_id: "B".into(),
                from: NodeState::Running,
                to: NodeState::Completed,
                timestamp: t0 + Duration::from_millis(20),
            },
            // C completes
            ExecutionEvent::StateChanged {
                node_id: "C".into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: t0 + Duration::from_millis(10),
            },
            ExecutionEvent::StateChanged {
                node_id: "C".into(),
                from: NodeState::Pending,
                to: NodeState::Running,
                timestamp: t0 + Duration::from_millis(10),
            },
            ExecutionEvent::NodeCompleted {
                node_id: "C".into(),
                outputs: Outputs::new(),
            },
            ExecutionEvent::StateChanged {
                node_id: "C".into(),
                from: NodeState::Running,
                to: NodeState::Completed,
                timestamp: t0 + Duration::from_millis(20),
            },
            // D completes
            ExecutionEvent::StateChanged {
                node_id: "D".into(),
                from: NodeState::Idle,
                to: NodeState::Pending,
                timestamp: t0 + Duration::from_millis(20),
            },
            ExecutionEvent::StateChanged {
                node_id: "D".into(),
                from: NodeState::Pending,
                to: NodeState::Running,
                timestamp: t0 + Duration::from_millis(20),
            },
            ExecutionEvent::NodeCompleted {
                node_id: "D".into(),
                outputs: Outputs::new(),
            },
            ExecutionEvent::StateChanged {
                node_id: "D".into(),
                from: NodeState::Running,
                to: NodeState::Completed,
                timestamp: t0 + Duration::from_millis(30),
            },
            ExecutionEvent::ExecutionCompleted {
                elapsed: Duration::from_millis(30),
            },
        ];

        let full_trace = ExecutionTrace::from_events(&events);
        assert_eq!(full_trace.records.len(), 4);

        // B's view: should only see A (its sole ancestor)
        let b_trace = full_trace.for_node("B", &graph);
        assert_eq!(b_trace.records.len(), 1);
        assert_eq!(b_trace.records[0].node_id, "A");

        // C's view: should only see A (not B)
        let c_trace = full_trace.for_node("C", &graph);
        assert_eq!(c_trace.records.len(), 1);
        assert_eq!(c_trace.records[0].node_id, "A");

        // D's view: should see A, B, C (all ancestors)
        let d_trace = full_trace.for_node("D", &graph);
        assert_eq!(d_trace.records.len(), 3);
        let d_ids: Vec<&str> = d_trace.execution_order();
        assert!(d_ids.contains(&"A"));
        assert!(d_ids.contains(&"B"));
        assert!(d_ids.contains(&"C"));

        // A's view: empty (no ancestors)
        let a_trace = full_trace.for_node("A", &graph);
        assert!(a_trace.records.is_empty());
    }

    // -- Integration test: trace from real executor --

    #[tokio::test]
    async fn trace_from_real_executor() {
        use crate::execute::{sync_handler, Executor, TopologicalExecutor};
        use crate::graph::node::Node;
        use crate::graph::Graph;

        let mut graph = Graph::new();
        graph
            .add_node(Node::new("A", "Start").with_handler("inc"))
            .unwrap();
        graph
            .add_node(Node::new("B", "Middle").with_handler("inc"))
            .unwrap();
        graph
            .add_node(Node::new("C", "End").with_handler("inc"))
            .unwrap();
        graph
            .add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();
        graph
            .add_edge(&"B".into(), "out", &"C".into(), "in", None)
            .unwrap();

        let mut handlers = crate::execute::HandlerRegistry::new();
        handlers.insert(
            "inc".into(),
            sync_handler(|_node, _inputs| {
                let mut out = Outputs::new();
                out.insert("value".into(), Value::I64(1));
                Ok(out)
            }),
        );

        let executor = TopologicalExecutor::new();
        let result = executor.execute(&graph, &handlers).await.unwrap();

        let trace = result.trace();
        assert_eq!(trace.records.len(), 3);
        assert_eq!(trace.completed_nodes().len(), 3);
        assert_eq!(trace.failed_nodes().len(), 0);

        // Verify execution order respects dependencies: A before B before C
        let order = trace.execution_order();
        let a_pos = order.iter().position(|&id| id == "A").unwrap();
        let b_pos = order.iter().position(|&id| id == "B").unwrap();
        let c_pos = order.iter().position(|&id| id == "C").unwrap();
        assert!(a_pos < b_pos);
        assert!(b_pos < c_pos);

        // Verify outputs were captured
        let a = trace.node("A").unwrap();
        assert_eq!(
            a.outputs.as_ref().unwrap().get("value"),
            Some(&Value::I64(1))
        );

        // Verify timing
        assert!(trace.elapsed > Duration::ZERO);
        for record in &trace.records {
            assert!(record.elapsed.is_some());
        }

        // Verify JSON serialization
        let json = trace.to_json().unwrap();
        let parsed: ExecutionTrace = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.records.len(), 3);
    }
}
