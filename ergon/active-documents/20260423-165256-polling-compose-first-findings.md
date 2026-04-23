20260423-165256-polling-compose-first-findings.md

# Polling Compose-First Findings

## 1. Composition Walkthrough

The example (`examples/poll_until_composed.mmd`) uses a `loop:` subgraph containing four nodes. `START` seeds an attempt counter. Inside the loop, `INVOKE` (a `rhai` node simulating a subgraph call) increments the counter and returns either `"pending"` or `"ready"` based on it. A `branch` node (`PREDICATE`) routes `yes` to `DONE` (loop exit) and `no` to `SLEEP` (`delay` handler, 500 ms fixed backoff). `SLEEP` feeds back to `INVOKE` for the next poll. The loop controller resets the body on the `no` path and exits when the exit node `DONE` completes. `exec.loop_max_iterations` on `INVOKE` caps the total attempts.

## 2. Authoring Friction

1. **Attempt counter is manual.** There is no iteration index exposed inside the loop body. The counter must be threaded through node outputs (`attempt` key passed through every node in the chain) and incremented by rhai. A real `poll_until` node would provide `attempt` implicitly.

2. **Predicate sees node outputs, not blackboard.** `LoopConfig::While` (the `loop_while` exec key) evaluates its guard against the blackboard *before* each iteration—it cannot inspect what the last poll returned via the output port. Using `branch` instead works, but requires a two-exit topology with one path as the "done" exit and one as "continue," forcing the author to reason about which exit node the loop controller watches for completion.

3. **Backoff is a naked constant.** Fixed delay requires wiring `SLEEP` manually and setting `delay_ms`. Exponential backoff requires a custom rhai expression to compute the delay and a blackboard-write path that does not exist in the rhai handler (rhai can read `ctx` but cannot write it). So exponential backoff is **not expressible** without a Rust handler change.

4. **Max-attempts and predicate are duplicate control paths.** Both the `branch` guard and `exec.loop_max_iterations` must be specified independently; there is no single place to say "stop on either condition." If the max is hit, the loop controller stops silently with no distinguished output—the author cannot tell on the success path whether termination was by predicate or by cap.

5. **"Ugh" threshold reached.** The graph is ~45 lines for a 3-node logical operation (poll, sleep, check). Every new attempt requires re-plumbing the counter through all intermediate nodes. If a real subgraph invocation node replaces the rhai stub, it adds another nesting level and the port-mapping becomes non-obvious.

## 3. Recommendation

**BUILD-BUT-NARROWER**

The composition works but produces noticeable friction at three seams: the manual counter, the absence of write-to-blackboard from rhai (blocks exponential backoff), and the dual-path termination with no unified exit signal. A minimal `poll_until` node solves all three without requiring a general scripting escape hatch.

## 4. Minimum Surface to Add

A single `poll_until` handler with these config knobs only:

- `graph` — name of the graph to invoke each iteration (delegates to `SubgraphInvocationHandler`)
- `predicate` — rhai expression evaluated against the subgraph's output map (truthy = done)
- `max_attempts` — hard cap; default 10
- `delay_ms` — fixed inter-attempt delay; default 0 (no sleep)

Outputs: all keys from the final invocation, plus `attempts_used: I64` and `timed_out: Bool`.

No exponential backoff, no jitter, no LLM guard — those can be added later if needed.
