use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::execute::context::ExecutionContext;
use crate::execute::lifecycle::NodeState;
use crate::graph::node::NodeId;
use crate::graph::{Graph, SubgraphDirective};

// ---------------------------------------------------------------------------
// LoopIterator — trait for the caller to control iteration
// ---------------------------------------------------------------------------

/// Trait that callers implement to control loop iteration for the SteppedExecutor.
///
/// The `LoopController` drives the state machine (detect end, reset, check bounds).
/// The `LoopIterator` provides domain-specific logic (resolve items, update variables).
pub trait LoopIterator: Send {
    /// Called when a loop begins. Return the total number of items to iterate,
    /// or `None` for unbounded loops.
    fn on_loop_start(&mut self, loop_id: &str, ctx: &ExecutionContext) -> Option<usize>;

    /// Called before each iteration. Return `true` to continue, `false` to stop.
    /// `index` is the 0-based iteration count.
    fn on_iteration(&mut self, loop_id: &str, index: usize, ctx: &ExecutionContext) -> bool;

    /// Called when the loop completes (all iterations done or stopped early).
    fn on_loop_end(&mut self, loop_id: &str, ctx: &ExecutionContext);
}

// ---------------------------------------------------------------------------
// LoopState — per-loop tracking
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopState {
    /// The loop subgraph ID.
    pub loop_id: String,
    /// All node IDs in the loop body (including entry and exit nodes).
    pub body_node_ids: Vec<String>,
    /// The entry node ID — first body node with incoming edges from outside.
    pub entry_node_id: String,
    /// Exit node IDs — body nodes with outgoing edges to outside.
    /// With conditional branching, multiple exit paths may exist;
    /// the loop advances when ANY exit node completes.
    pub exit_node_ids: Vec<String>,
    /// Current iteration index (0-based).
    pub index: usize,
    /// Total items (if known from on_loop_start).
    pub total: Option<usize>,
    /// Maximum iterations (safety cap).
    pub max_iterations: usize,
    /// Whether the loop has been initialized (on_loop_start called).
    pub initialized: bool,
}

// ---------------------------------------------------------------------------
// LoopController
// ---------------------------------------------------------------------------

/// Manages loop iteration for the SteppedExecutor.
///
/// Usage:
/// ```ignore
/// loop {
///     loop_controller.prepare(graph, &ctx, &mut iterator);
///     let tick = executor.tick(graph, handlers, &ctx).await?;
///     let reset = loop_controller.process_tick(&ctx, &mut iterator);
///     if tick.is_complete && !reset { break; }
/// }
/// ```
pub struct LoopController {
    loops: HashMap<String, LoopState>,
}

impl LoopController {
    /// Create a new LoopController with pre-built loop states.
    pub fn new(loops: HashMap<String, LoopState>) -> Self {
        Self { loops }
    }

    /// Create an empty LoopController (no loops).
    pub fn empty() -> Self {
        Self {
            loops: HashMap::new(),
        }
    }

    /// Build a LoopController from all `loop:` subgraphs in the graph.
    ///
    /// Uses subgraph topology analysis to determine entry/exit nodes
    /// instead of requiring sentinel nodes.
    pub fn from_subgraphs(graph: &Graph) -> Self {
        let mut loops = HashMap::new();
        for sg in graph.subgraphs() {
            if sg.directive != SubgraphDirective::Loop {
                continue;
            }
            let topo = graph.subgraph_topology(sg);
            let Some(first_node) = sg.nodes.first().cloned() else {
                continue; // Empty subgraph, skip
            };
            let entry = topo
                .entry_nodes
                .first()
                .cloned()
                .unwrap_or_else(|| first_node.clone());
            let exit_ids: Vec<String> = if topo.exit_nodes.is_empty() {
                vec![entry.0.clone()]
            } else {
                topo.exit_nodes.iter().map(|n| n.0.clone()).collect()
            };

            loops.insert(
                sg.id.clone(),
                LoopState {
                    loop_id: sg.id.clone(),
                    body_node_ids: sg.all_node_ids().iter().map(|n| n.0.clone()).collect(),
                    entry_node_id: entry.0,
                    exit_node_ids: exit_ids,
                    index: 0,
                    total: None,
                    max_iterations: 1000,
                    initialized: false,
                },
            );
        }
        Self { loops }
    }

    /// Get the loop states (for serialization/persistence).
    pub fn states(&self) -> &HashMap<String, LoopState> {
        &self.loops
    }

    /// Get mutable access to loop states (for deserialization/restore).
    pub fn states_mut(&mut self) -> &mut HashMap<String, LoopState> {
        &mut self.loops
    }

    /// Pre-tick: initialize loops whose entry node is about to run.
    ///
    /// A loop is ready to initialize when all predecessors of its entry node
    /// (outside the subgraph) are in terminal state. Call this before each
    /// tick so that `on_loop_start` and `on_iteration(0)` fire before the
    /// entry node executes.
    pub fn prepare(
        &mut self,
        graph: &Graph,
        ctx: &ExecutionContext,
        iterator: &mut dyn LoopIterator,
    ) {
        for (loop_id, state) in &mut self.loops {
            if state.initialized {
                continue;
            }
            // Check if entry node is still idle and predecessors are done
            if ctx.get_state(&state.entry_node_id) != NodeState::Idle {
                continue;
            }
            let entry_nid = NodeId::new(&state.entry_node_id);
            let preds = graph.predecessors(&entry_nid);
            let all_preds_done = preds.is_empty()
                || preds
                    .iter()
                    .all(|p| ctx.get_state(&p.id.0).is_terminal());
            if !all_preds_done {
                continue;
            }

            let total = iterator.on_loop_start(loop_id, ctx);
            state.initialized = true;
            state.total = total;

            if !iterator.on_iteration(loop_id, 0, ctx) {
                // Zero-iteration loop: cancel all body nodes so the tick
                // doesn't execute them, then signal completion.
                for nid in &state.body_node_ids {
                    if ctx.get_state(nid) == NodeState::Idle {
                        let _ = ctx.set_state(nid, NodeState::Cancelled);
                    }
                }
                for exit_id in &state.exit_node_ids {
                    if ctx.get_state(exit_id) == NodeState::Idle {
                        let _ = ctx.set_state(exit_id, NodeState::Cancelled);
                    }
                }
                iterator.on_loop_end(loop_id, ctx);
            }
        }
    }

    /// Post-tick: detect exit node completions and advance iterations.
    ///
    /// Returns `true` if any loop body was reset (meaning the graph is no
    /// longer complete and another tick should be performed).
    pub fn process_tick(
        &mut self,
        ctx: &ExecutionContext,
        iterator: &mut dyn LoopIterator,
    ) -> bool {
        let mut reset_occurred = false;

        let loop_ids: Vec<String> = self.loops.keys().cloned().collect();
        for loop_id in &loop_ids {
            let state = &self.loops[loop_id];
            if !state.initialized {
                continue;
            }

            // Check if ANY exit node completed (with conditional branching,
            // one exit path completes while others may be cancelled).
            let any_exit_completed = state
                .exit_node_ids
                .iter()
                .any(|id| ctx.get_state(id) == NodeState::Completed);
            if !any_exit_completed {
                continue;
            }

            let state = self.loops.get_mut(loop_id).unwrap();
            state.index += 1;

            let at_max = state.index >= state.max_iterations;
            let at_end = state.total.is_some_and(|t| state.index >= t);

            if !at_max && !at_end && iterator.on_iteration(loop_id, state.index, ctx) {
                // More iterations: reset body + all exit nodes.
                let mut ids_to_reset: Vec<&str> = state
                    .body_node_ids
                    .iter()
                    .map(|s| s.as_str())
                    .collect();
                for exit_id in &state.exit_node_ids {
                    if !ids_to_reset.contains(&exit_id.as_str()) {
                        ids_to_reset.push(exit_id.as_str());
                    }
                }
                ctx.reset_states(ids_to_reset.into_iter());
                reset_occurred = true;
            } else {
                iterator.on_loop_end(loop_id, ctx);
            }
        }

        reset_occurred
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::graph::node::Node;
    use crate::graph::Subgraph;

    use crate::execute::concurrency::ConcurrencyLimits;
    use crate::execute::context::CancellationToken;

    fn make_ctx() -> Arc<ExecutionContext> {
        Arc::new(ExecutionContext::with_concurrency(
            CancellationToken::new(),
            ConcurrencyLimits::new(),
        ))
    }

    fn make_loop_state() -> LoopState {
        LoopState {
            loop_id: "sg-loop1".into(),
            body_node_ids: vec!["body".into()],
            entry_node_id: "body".into(),
            exit_node_ids: vec!["body".into()],
            index: 0,
            total: None,
            max_iterations: 100,
            initialized: false,
        }
    }

    struct CountingIterator {
        items: Vec<String>,
        started: bool,
        ended: bool,
    }

    impl CountingIterator {
        fn new(items: Vec<String>) -> Self {
            Self {
                items,
                started: false,
                ended: false,
            }
        }
    }

    impl LoopIterator for CountingIterator {
        fn on_loop_start(&mut self, _loop_id: &str, _ctx: &ExecutionContext) -> Option<usize> {
            self.started = true;
            Some(self.items.len())
        }

        fn on_iteration(&mut self, _loop_id: &str, index: usize, _ctx: &ExecutionContext) -> bool {
            index < self.items.len()
        }

        fn on_loop_end(&mut self, _loop_id: &str, _ctx: &ExecutionContext) {
            self.ended = true;
        }
    }

    #[test]
    fn prepare_initializes_when_predecessors_done() {
        // pred --> body --> succ
        // loop subgraph contains [body]
        let mut g = Graph::new();
        for id in ["pred", "body", "succ"] {
            g.add_node(Node::new(id, id)).unwrap();
        }
        g.add_edge(&"pred".into(), "", &"body".into(), "", None).unwrap();
        g.add_edge(&"body".into(), "", &"succ".into(), "", None).unwrap();
        g.add_subgraph(Subgraph {
            id: "sg-loop1".into(),
            label: "loop: process".into(),
            directive: SubgraphDirective::Loop,
            nodes: vec!["body".into()],
            children: Vec::new(),
        });

        let ctx = make_ctx();
        let mut controller = LoopController::from_subgraphs(&g);
        let mut iter = CountingIterator::new(vec!["a".into(), "b".into()]);

        // Before predecessor completes: prepare does nothing
        controller.prepare(&g, &ctx, &mut iter);
        assert!(!iter.started);

        // Predecessor completes
        let _ = ctx.set_state("pred", NodeState::Pending);
        let _ = ctx.set_state("pred", NodeState::Running);
        let _ = ctx.set_state("pred", NodeState::Completed);

        // Now prepare initializes the loop
        controller.prepare(&g, &ctx, &mut iter);
        assert!(iter.started);
        assert!(controller.states()["sg-loop1"].initialized);
        assert_eq!(controller.states()["sg-loop1"].total, Some(2));
    }

    #[test]
    fn process_tick_resets_on_exit_completion() {
        let ctx = make_ctx();
        let mut loops = HashMap::new();
        let mut state = make_loop_state();
        state.initialized = true;
        state.total = Some(3);
        loops.insert("sg-loop1".into(), state);
        let mut controller = LoopController::new(loops);
        let mut iter = CountingIterator::new(vec!["a".into(), "b".into(), "c".into()]);

        // Exit node completes (first iteration done)
        let _ = ctx.set_state("body", NodeState::Pending);
        let _ = ctx.set_state("body", NodeState::Running);
        let _ = ctx.set_state("body", NodeState::Completed);

        let reset = controller.process_tick(&ctx, &mut iter);

        assert!(reset, "should have reset body nodes");
        assert_eq!(controller.states()["sg-loop1"].index, 1);
        // body should have been reset to Idle
        assert_eq!(ctx.get_state("body"), NodeState::Idle);
    }

    #[test]
    fn process_tick_ends_at_total() {
        let ctx = make_ctx();
        let mut loops = HashMap::new();
        let mut state = make_loop_state();
        state.initialized = true;
        state.total = Some(2);
        state.index = 1; // About to complete the last iteration
        loops.insert("sg-loop1".into(), state);
        let mut controller = LoopController::new(loops);
        let mut iter = CountingIterator::new(vec!["a".into(), "b".into()]);

        // Exit node completes (second/final iteration)
        let _ = ctx.set_state("body", NodeState::Pending);
        let _ = ctx.set_state("body", NodeState::Running);
        let _ = ctx.set_state("body", NodeState::Completed);

        let reset = controller.process_tick(&ctx, &mut iter);

        assert!(!reset, "should NOT reset — loop is done");
        assert!(iter.ended);
    }

    #[test]
    fn process_tick_respects_max_iterations() {
        let ctx = make_ctx();
        let mut loops = HashMap::new();
        let mut state = make_loop_state();
        state.initialized = true;
        state.total = Some(100); // Many items
        state.max_iterations = 2; // But capped at 2
        state.index = 1; // About to hit the cap
        loops.insert("sg-loop1".into(), state);
        let mut controller = LoopController::new(loops);
        let mut iter = CountingIterator::new((0..100).map(|i| format!("{i}")).collect());

        let _ = ctx.set_state("body", NodeState::Pending);
        let _ = ctx.set_state("body", NodeState::Running);
        let _ = ctx.set_state("body", NodeState::Completed);

        let reset = controller.process_tick(&ctx, &mut iter);

        assert!(!reset, "should NOT reset — hit max_iterations");
        assert!(iter.ended);
    }

    #[test]
    fn from_subgraphs_builds_from_topology() {
        // pred --> A --> B --> succ
        // loop subgraph contains [A, B]
        let mut g = Graph::new();
        for id in ["pred", "A", "B", "succ"] {
            g.add_node(Node::new(id, id)).unwrap();
        }
        g.add_edge(&"pred".into(), "", &"A".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"B".into(), "", None).unwrap();
        g.add_edge(&"B".into(), "", &"succ".into(), "", None).unwrap();
        g.add_subgraph(Subgraph {
            id: "loop1".into(),
            label: "loop: process".into(),
            directive: SubgraphDirective::Loop,
            nodes: vec!["A".into(), "B".into()],
            children: Vec::new(),
        });
        // Non-loop subgraph should be ignored
        g.add_subgraph(Subgraph {
            id: "par1".into(),
            label: "parallel: other".into(),
            directive: SubgraphDirective::Parallel,
            nodes: vec!["pred".into()],
            children: Vec::new(),
        });

        let controller = LoopController::from_subgraphs(&g);
        assert_eq!(controller.states().len(), 1);

        let state = &controller.states()["loop1"];
        assert_eq!(state.entry_node_id, "A");
        assert_eq!(state.exit_node_ids, vec!["B"]);
        assert_eq!(state.body_node_ids, vec!["A", "B"]);
        assert!(!state.initialized);
    }

    #[test]
    fn from_subgraphs_single_node_loop() {
        // pred --> A --> succ, loop subgraph = [A]
        let mut g = Graph::new();
        for id in ["pred", "A", "succ"] {
            g.add_node(Node::new(id, id)).unwrap();
        }
        g.add_edge(&"pred".into(), "", &"A".into(), "", None).unwrap();
        g.add_edge(&"A".into(), "", &"succ".into(), "", None).unwrap();
        g.add_subgraph(Subgraph {
            id: "loop1".into(),
            label: "loop: single".into(),
            directive: SubgraphDirective::Loop,
            nodes: vec!["A".into()],
            children: Vec::new(),
        });

        let controller = LoopController::from_subgraphs(&g);
        let state = &controller.states()["loop1"];
        assert_eq!(state.entry_node_id, "A");
        assert_eq!(state.exit_node_ids, vec!["A"]);
        assert_eq!(state.body_node_ids, vec!["A"]);
    }

    #[test]
    fn zero_iteration_loop_cancels_body() {
        // pred --> body --> succ, loop over empty list
        let mut g = Graph::new();
        for id in ["pred", "body", "succ"] {
            g.add_node(Node::new(id, id)).unwrap();
        }
        g.add_edge(&"pred".into(), "", &"body".into(), "", None).unwrap();
        g.add_edge(&"body".into(), "", &"succ".into(), "", None).unwrap();
        g.add_subgraph(Subgraph {
            id: "loop1".into(),
            label: "loop: empty".into(),
            directive: SubgraphDirective::Loop,
            nodes: vec!["body".into()],
            children: Vec::new(),
        });

        let ctx = make_ctx();
        let mut controller = LoopController::from_subgraphs(&g);
        // Empty list → on_iteration(0) returns false
        let mut iter = CountingIterator::new(vec![]);

        // Predecessor completes
        let _ = ctx.set_state("pred", NodeState::Pending);
        let _ = ctx.set_state("pred", NodeState::Running);
        let _ = ctx.set_state("pred", NodeState::Completed);

        controller.prepare(&g, &ctx, &mut iter);

        assert!(iter.started);
        assert!(iter.ended);
        // Body should be cancelled, not idle
        assert_eq!(ctx.get_state("body"), NodeState::Cancelled);
    }

    #[test]
    fn prepare_root_entry_no_predecessors() {
        // body --> succ (body is a root node, no predecessors)
        let mut g = Graph::new();
        for id in ["body", "succ"] {
            g.add_node(Node::new(id, id)).unwrap();
        }
        g.add_edge(&"body".into(), "", &"succ".into(), "", None).unwrap();
        g.add_subgraph(Subgraph {
            id: "loop1".into(),
            label: "loop: root".into(),
            directive: SubgraphDirective::Loop,
            nodes: vec!["body".into()],
            children: Vec::new(),
        });

        let ctx = make_ctx();
        let mut controller = LoopController::from_subgraphs(&g);
        let mut iter = CountingIterator::new(vec!["a".into(), "b".into()]);

        // No predecessors → should initialize immediately
        controller.prepare(&g, &ctx, &mut iter);

        assert!(iter.started);
        assert!(controller.states()["loop1"].initialized);
        assert_eq!(controller.states()["loop1"].total, Some(2));
    }

    #[test]
    fn multiple_exit_nodes_any_completed_advances_loop() {
        // Loop with two exit paths (conditional branching):
        //   pred --> entry --> exit_a --> after
        //                 \-> exit_b --> after
        // exit_a is the "break" path (e.g., approved)
        // exit_b is the "continue" path (e.g., else)
        let mut g = Graph::new();
        for id in ["pred", "entry", "exit_a", "exit_b", "after"] {
            g.add_node(Node::new(id, id)).unwrap();
        }
        g.add_edge(&"pred".into(), "", &"entry".into(), "", None).unwrap();
        g.add_edge(&"entry".into(), "", &"exit_a".into(), "", None).unwrap();
        g.add_edge(&"entry".into(), "", &"exit_b".into(), "", None).unwrap();
        g.add_edge(&"exit_a".into(), "", &"after".into(), "", None).unwrap();
        g.add_edge(&"exit_b".into(), "", &"after".into(), "", None).unwrap();

        g.add_subgraph(Subgraph {
            id: "loop1".into(),
            label: "loop: iter".into(),
            directive: SubgraphDirective::Loop,
            nodes: vec!["entry".into(), "exit_a".into(), "exit_b".into()],
            children: Vec::new(),
        });

        let ctx = make_ctx();
        let mut controller = LoopController::from_subgraphs(&g);
        let mut iter = CountingIterator::new(vec!["a".into(), "b".into(), "c".into()]);

        // Should detect both exit nodes
        let state = &controller.states()["loop1"];
        assert_eq!(state.exit_node_ids.len(), 2);
        assert!(state.exit_node_ids.contains(&"exit_a".to_string()));
        assert!(state.exit_node_ids.contains(&"exit_b".to_string()));

        // Helper to transition node to a terminal state
        let set_completed = |id: &str| {
            ctx.set_state(id, NodeState::Pending).unwrap();
            ctx.set_state(id, NodeState::Running).unwrap();
            ctx.set_state(id, NodeState::Completed).unwrap();
        };
        let set_cancelled = |id: &str| {
            ctx.set_state(id, NodeState::Pending).unwrap();
            ctx.set_state(id, NodeState::Cancelled).unwrap();
        };

        // Initialize
        set_completed("pred");
        controller.prepare(&g, &ctx, &mut iter);
        assert!(controller.states()["loop1"].initialized);

        // Simulate: exit_a cancelled (branch blocked), exit_b completed (else path)
        set_completed("entry");
        set_cancelled("exit_a");
        set_completed("exit_b");

        // process_tick should detect exit_b completed and advance
        let reset = controller.process_tick(&ctx, &mut iter);
        assert!(reset, "loop should reset when any exit node completes");
        assert_eq!(controller.states()["loop1"].index, 1);

        // Body nodes should be reset to Idle
        assert_eq!(ctx.get_state("entry"), NodeState::Idle);
        assert_eq!(ctx.get_state("exit_a"), NodeState::Idle);
        assert_eq!(ctx.get_state("exit_b"), NodeState::Idle);
    }

    #[test]
    fn nested_subgraph_nodes_included_in_body() {
        let mut g = Graph::new();
        for id in ["entry", "inner_a", "inner_b", "join", "after"] {
            g.add_node(Node::new(id, id)).unwrap();
        }
        g.add_edge(&"entry".into(), "", &"inner_a".into(), "", None).unwrap();
        g.add_edge(&"entry".into(), "", &"inner_b".into(), "", None).unwrap();
        g.add_edge(&"inner_a".into(), "", &"join".into(), "", None).unwrap();
        g.add_edge(&"inner_b".into(), "", &"join".into(), "", None).unwrap();
        g.add_edge(&"join".into(), "", &"after".into(), "", None).unwrap();

        g.add_subgraph(Subgraph {
            id: "outer_loop".into(),
            label: "loop: outer".into(),
            directive: SubgraphDirective::Loop,
            nodes: vec!["entry".into(), "join".into()],
            children: vec![Subgraph {
                id: "inner_parallel".into(),
                label: "parallel: inner".into(),
                directive: SubgraphDirective::Parallel,
                nodes: vec!["inner_a".into(), "inner_b".into()],
                children: Vec::new(),
            }],
        });

        let controller = LoopController::from_subgraphs(&g);
        let state = &controller.states()["outer_loop"];

        // body_node_ids must include nodes from nested child subgraphs
        assert!(state.body_node_ids.contains(&"entry".into()));
        assert!(state.body_node_ids.contains(&"join".into()));
        assert!(state.body_node_ids.contains(&"inner_a".into()));
        assert!(state.body_node_ids.contains(&"inner_b".into()));
        assert_eq!(state.body_node_ids.len(), 4);
    }
}
