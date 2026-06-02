20260602-100350-Composition-Handlers-Followups.md

# psflow composition handlers — reference doc + claude-workflow-as-a-node follow-ups

## 1. Preamble

### 1.1 Context

Follows the session that added psflow's composition primitives (`map`, `loop`, plus the pre-existing `poll_until` and `subgraph_invoke`) to close the dynamic-control-flow gap identified when comparing psflow to Claude Code's dynamic workflows.

### 1.2 Purpose

Cold-start handoff for the two remaining composition tasks: writing a user-facing reference doc for the four composition handlers, and extending the set with a "Claude-workflow-as-a-node" handler plus hardening.

## 2. Scope

### 2.1 In scope

- A reference doc for the `map` / `loop` / `poll_until` / `subgraph_invoke` handlers (annotation surface + quality-pattern recipes).
- A `claude_workflow` handler that runs a Claude Code dynamic workflow as a psflow step.
- Hardening of `map`/`loop` (additional reducers, `while` termination, the nested-concurrent depth-guard subtlety).

### 2.2 Out of scope

- The four handlers themselves — implemented, tested, and wired this session.
- The runner infrastructure (named graphs, runtime inputs, cross-run state, scheduling, caching, notify, event bridge).
- The Composio integration and its replacement — tracked separately; isolated as a removable leaf.

## 3. Status

The composition handlers are implemented, unit-tested, and wired into `psflow-run`, verified end-to-end (`map-demo` → squares; `loop-demo` → accumulates to a target). The two items here are not started. Nothing blocks them; they are the next increment.

## 4. Body

### 4.1 The composition handlers (current state)

All four are `NodeHandler`s composed from `subgraph_invoke`'s machinery (`GraphLibrary`, `execute_child`, the deferred handler-registry slot, the script engine, context inheritance). None required engine or executor changes. The runner's `build_handlers` loads every `.mmd` in the graphs dir as a named subgraph (the library) and registers all four with deferred registry slots set after the registry is finalized, so subgraphs can invoke each other and recurse.

- `subgraph_invoke` — runs a named subgraph as a function. Config: `graph`; `exec.max_depth`, `exec.context_inheritance`. Parent inputs inject into the subgraph's source nodes; sink outputs return merged.
- `map` — data-driven fan-out: runs a subgraph once per element of a runtime list, concurrently and order-preserved, then reduces. Config: `over` (input key holding the list), `graph`, `as` (element binding, default `item`), `max_concurrency`, `reduce` (`collect` → `results`/`count`; `quorum` → `votes`/`passed` over a boolean field), `on_item_error` (`skip`/`fail`). Lives in the `map` module.
- `loop` — accumulating loop generalizing `poll_until`. Each iteration runs a subgraph, appends its produced items to a growing (optionally deduped) collection, and injects that collection back into the next iteration as `state`. Config: `graph`, `collect` (output key with the per-round list), `until` (Rhai over `state`/`iteration`/`output`), `until_dry` (N empty rounds), `dedup_key` (Rhai over `item`), `max_iterations`, `delay_ms`, `state_as`. Outputs `collected`/`count`/`iterations`/`dry_rounds`/`stopped_by`. Lives in the `loop_handler` module.
- `poll_until` — fixed-attempt loop until a Rhai predicate over the subgraph output, or a cap. Config: `graph`, `predicate`, `max_attempts`, `delay_ms`.

### 4.2 Quality-pattern recipes the reference doc should capture

These are the Claude-dynamic-workflow patterns, expressed declaratively with the handlers above:

- Fan-out + adversarial verify → `map` with `reduce: quorum` over a panel of diverse-lens judge subgraphs.
- Loop-until-dry / accumulate-to-target → `loop` with `until_dry` or `until: "len(state) >= N"`.
- Find → dedup → panel → loop → `loop` (with `dedup_key`) whose subgraph runs a `map` verify step each round.

### 4.3 claude-workflow-as-a-node (item for §9.1)

The remaining expressiveness frontier is truly-arbitrary imperative orchestration that doesn't fit `map`/`loop`. The proposed shape: a `claude_workflow` handler that invokes a Claude Code dynamic workflow (bundled like `/deep-research`, or a saved one) as a single psflow node — psflow owns the durable/triggered/scheduled outside, the agent swarm runs inside, and the node returns the workflow's final result as structured outputs. Feasibility is unverified (see §6): the dynamic-workflow runtime is a Claude Code feature, and whether it is invocable from a standalone process / headless `claude -p` rather than only an interactive session is the open question that gates this item.

## 5. Decisions

- Encapsulate dynamism in handlers, keep the graph declarative. Rationale: preserves psflow's renderable/inspectable/durable graph while gaining runtime-dynamic behavior. Rejected alternative: going imperative (a JS-style script), which is what Claude workflows do and what sacrifices the visual graph.
- Compose from `subgraph_invoke`, no engine changes. Rationale: `map`/`loop` are leaf handlers over existing machinery; lower risk, no executor surface area. Rejected alternative: a new dynamic-fan-out executor.
- Voting is `map` + `reduce: quorum`, not a separate primitive. Rationale: adversarial-verify falls out of fan-out for free.
- `loop` generalizes `poll_until` rather than replacing it. Rationale: `poll_until` stays the minimal fixed-attempt case; `loop` adds accumulation and flexible termination.

## 6. Open questions

- [ ] Is a Claude Code dynamic workflow invocable from a standalone process (headless `claude -p` or the Agent SDK) so a `claude_workflow` handler can call it, or is it interactive-session-only? Resolving this decides whether §4.3 is buildable as designed or needs a different bridge.
- [ ] How should `map`'s reducers extend — a named `ResultReducer` registry, an accumulate-to-blackboard mode, or both? Resolving this sets the `reduce` config surface before more callers depend on `collect`/`quorum`.

## 7. Usage

Author a subgraph as any `.mmd` in the graphs dir (its file stem is its library name). Reference it from another graph via the `map`/`loop`/`poll_until`/`subgraph_invoke` handler annotations (config keys in §4.1). Run with `psflow-run <graph>` or `just graph <graph>`. The accumulated `state` (loop) and per-element binding (map) arrive as node inputs the subgraph reads. Recursion and mutual reference work because the registry is finalized before the deferred slots are set.

## 8. Known limitations

- `map.reduce` supports only `collect` and `quorum`. Promote when a workflow needs a custom reduction (named `ResultReducer`) or accumulate-to-blackboard.
- `loop` supports `until` (stop-when-true) but not `while` (continue-while-true). Promote when a graph reads more naturally as a while-loop.
- The `DepthGuard` counts nesting but is shared across concurrently-running siblings, so deeply nested **and** wide `map`/`loop` compositions can trip `max_depth` on breadth rather than true recursion depth. Promote when a real nested-concurrent composition hits a spurious depth error; the fix is to distinguish breadth from depth (e.g., per-branch depth context).

## 9. Plan

### 9.1 Planned tasks

- [ ] Write the composition reference doc covering `map` / `loop` / `poll_until` / `subgraph_invoke`: per-handler config surface and outputs (from §4.1) plus the quality-pattern recipes (§4.2). Place under `docs/` alongside the mermaid annotation reference.
- [ ] Resolve the §6 feasibility question for `claude_workflow` (headless / Agent SDK invocation), then build the `claude_workflow` handler per §4.3 (or adjust the design to the available invocation path). depends-on: the reference doc is independent and can land first.
- [ ] Harden `map`/`loop` per §8: add a custom-reducer path to `map`, a `while` termination to `loop`, and fix the breadth-vs-depth `DepthGuard` accounting. depends-on: only act on the items §8 flags as actually hit, not preemptively.

### 9.2 Audit fixes

_None._

### 9.3 Phased plan

_None._

## 10. Calibration notes

_None._

## 11. History

### 11.1 Already landed

- `6c42fa74` — `map` handler (data-driven fan-out over a runtime list).
- `d9ae0cb0` — wired `map` + `subgraph_invoke` into `psflow-run`; `map` verified end-to-end.
- `33854f95` — `loop` handler (accumulate + `until`/`until_dry`/`max`); wired the loop family (`loop` + `poll_until`) into the runner; `loop` verified end-to-end.

### 11.2 Deferred / superseded

_None._

## 12. Appendix

_None._

## 13. Related

- Handler sources: the `map`, `loop_handler`, `poll_until`, and `subgraph_invoke` modules under `src/handlers/`.
- Runner wiring: `build_handlers` / `load_graph_library` in the `psflow-run` binary.
- Example graphs: `map-demo` + `square_item`, `loop-demo` + `find_more` in the graphs dir.
- Claude Code dynamic workflows (the comparison that motivated this work): https://code.claude.com/docs/en/workflows
