use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::execute::context::ExecutionContext;
use crate::execute::lifecycle::NodeState;

// ---------------------------------------------------------------------------
// LoopIterator — trait for the caller to control iteration
// ---------------------------------------------------------------------------

/// Trait that callers implement to control loop iteration for the SteppedExecutor.
///
/// The `LoopController` drives the state machine (detect end, reset, check bounds).
/// The `LoopIterator` provides domain-specific logic (resolve items, update variables).
pub trait LoopIterator: Send {
    /// Called when a loop begins (loop-start node completes).
    /// Return the total number of items to iterate, or `None` for unbounded loops.
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
    /// The loop subgraph ID (e.g., "sg-loop1").
    pub loop_id: String,
    /// Node IDs in the loop body (excluding start/end sentinels).
    pub body_node_ids: Vec<String>,
    /// The loop-start sentinel node ID.
    pub start_node_id: String,
    /// The loop-end sentinel node ID.
    pub end_node_id: String,
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
/// After each `tick()`, call `process_tick()` to check if any loop-end nodes
/// completed and reset body nodes for the next iteration.
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

    /// Get the loop states (for serialization/persistence).
    pub fn states(&self) -> &HashMap<String, LoopState> {
        &self.loops
    }

    /// Get mutable access to loop states (for deserialization/restore).
    pub fn states_mut(&mut self) -> &mut HashMap<String, LoopState> {
        &mut self.loops
    }

    /// Check for loop-start and loop-end completions after a tick.
    ///
    /// Returns `true` if any loop body was reset (meaning the graph is no
    /// longer complete and another tick should be performed).
    pub fn process_tick(
        &mut self,
        ctx: &ExecutionContext,
        iterator: &mut dyn LoopIterator,
    ) -> bool {
        let mut reset_occurred = false;

        // Check for loop-start completions (initialize loops)
        let loop_ids: Vec<String> = self.loops.keys().cloned().collect();
        for loop_id in &loop_ids {
            let state = &self.loops[loop_id];
            if !state.initialized && ctx.get_state(&state.start_node_id) == NodeState::Completed {
                let total = iterator.on_loop_start(loop_id, ctx);
                let state = self.loops.get_mut(loop_id).unwrap();
                state.initialized = true;
                state.total = total;

                // Signal the first iteration
                if !iterator.on_iteration(loop_id, 0, ctx) {
                    // Iterator says don't run — mark loop as done
                    iterator.on_loop_end(loop_id, ctx);
                }
            }
        }

        // Check for loop-end completions (advance iteration)
        for loop_id in &loop_ids {
            let end_node_id = self.loops[loop_id].end_node_id.clone();
            let end_state = ctx.get_state(&end_node_id);

            if end_state == NodeState::Completed {
                let state = self.loops.get_mut(loop_id).unwrap();
                state.index += 1;

                let at_max = state.index >= state.max_iterations;
                let at_end = state.total.is_some_and(|t| state.index >= t);

                if !at_max && !at_end && iterator.on_iteration(loop_id, state.index, ctx) {
                    // More iterations: reset body nodes + end node
                    let ids_to_reset: Vec<String> = state
                        .body_node_ids
                        .iter()
                        .chain(std::iter::once(&state.end_node_id))
                        .cloned()
                        .collect();

                    ctx.reset_states(ids_to_reset.iter().map(|s| s.as_str()));
                    reset_occurred = true;
                } else {
                    // Loop done
                    iterator.on_loop_end(loop_id, ctx);
                }
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
            body_node_ids: vec!["loop1/body".into()],
            start_node_id: "loop1/loop-start".into(),
            end_node_id: "loop1/loop-end".into(),
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
    fn loop_controller_initializes_on_start_completion() {
        let ctx = make_ctx();
        let mut loops = HashMap::new();
        loops.insert("sg-loop1".into(), make_loop_state());
        let mut controller = LoopController::new(loops);
        let mut iter = CountingIterator::new(vec!["a".into(), "b".into()]);

        // Simulate loop-start completing
        let _ = ctx.set_state("loop1/loop-start", NodeState::Pending);
        let _ = ctx.set_state("loop1/loop-start", NodeState::Running);
        let _ = ctx.set_state("loop1/loop-start", NodeState::Completed);

        controller.process_tick(&ctx, &mut iter);

        assert!(iter.started);
        assert!(controller.states()["sg-loop1"].initialized);
        assert_eq!(controller.states()["sg-loop1"].total, Some(2));
    }

    #[test]
    fn loop_controller_resets_on_end_completion() {
        let ctx = make_ctx();
        let mut loops = HashMap::new();
        let mut state = make_loop_state();
        state.initialized = true;
        state.total = Some(3);
        loops.insert("sg-loop1".into(), state);
        let mut controller = LoopController::new(loops);
        let mut iter = CountingIterator::new(vec!["a".into(), "b".into(), "c".into()]);

        // Simulate loop-end completing (first iteration done)
        let _ = ctx.set_state("loop1/loop-end", NodeState::Pending);
        let _ = ctx.set_state("loop1/loop-end", NodeState::Running);
        let _ = ctx.set_state("loop1/loop-end", NodeState::Completed);

        let reset = controller.process_tick(&ctx, &mut iter);

        assert!(reset, "should have reset body nodes");
        assert_eq!(controller.states()["sg-loop1"].index, 1);
        // loop-end should have been reset to Idle
        assert_eq!(ctx.get_state("loop1/loop-end"), NodeState::Idle);
    }

    #[test]
    fn loop_controller_ends_at_total() {
        let ctx = make_ctx();
        let mut loops = HashMap::new();
        let mut state = make_loop_state();
        state.initialized = true;
        state.total = Some(2);
        state.index = 1; // About to complete the last iteration
        loops.insert("sg-loop1".into(), state);
        let mut controller = LoopController::new(loops);
        let mut iter = CountingIterator::new(vec!["a".into(), "b".into()]);

        // Simulate loop-end completing (second/final iteration)
        let _ = ctx.set_state("loop1/loop-end", NodeState::Pending);
        let _ = ctx.set_state("loop1/loop-end", NodeState::Running);
        let _ = ctx.set_state("loop1/loop-end", NodeState::Completed);

        let reset = controller.process_tick(&ctx, &mut iter);

        assert!(!reset, "should NOT reset — loop is done");
        assert!(iter.ended);
    }

    #[test]
    fn loop_controller_respects_max_iterations() {
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

        let _ = ctx.set_state("loop1/loop-end", NodeState::Pending);
        let _ = ctx.set_state("loop1/loop-end", NodeState::Running);
        let _ = ctx.set_state("loop1/loop-end", NodeState::Completed);

        let reset = controller.process_tick(&ctx, &mut iter);

        assert!(!reset, "should NOT reset — hit max_iterations");
        assert!(iter.ended);
    }
}
