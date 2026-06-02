# Composition Handlers Reference

Reference for the four handlers that add runtime-dynamic control flow while
keeping the graph declarative: `subgraph_invoke`, `map`, `loop`, and
`poll_until`. Ctrl-F for the field you need.

All four invoke a **named subgraph** — any other `.mmd` in the graphs dir, looked
up by its file stem (`square_item.mmd` → `square_item`). They compose the same
machinery (`GraphLibrary`, `execute_child`, the depth guard, context
inheritance), so what you learn about one transfers to the rest.

New to authoring flows? Start with [writing-flow-mmd.md](./writing-flow-mmd.md),
then use [mermaid-annotation-reference.md](./mermaid-annotation-reference.md) for
the leaf-handler config surface and this page for the composition layer.

---

## 1. The shared subgraph model

A subgraph is an ordinary flow with no special markup. The composition handler
treats it as a function:

- **Inputs in** — the parent's inputs are injected into the subgraph's **source
  nodes** (nodes with no predecessors). One source → it receives all parent
  inputs. Multiple sources → an input key matching a source node ID goes to that
  node; otherwise each source receives all parent inputs.
- **Outputs out** — the subgraph's **sink nodes** (no successors) have their
  outputs merged (last-writer-wins) and returned to the parent.
- **Each handler injects extra inputs** on top of the parent inputs: `map` adds
  the per-element binding (`item`), `loop` adds the accumulated `state`.

Because the handler registry is finalized before each handler's deferred slot is
set, subgraphs can invoke each other and recurse. A subgraph can itself contain
`map`/`loop`/`poll_until`/`subgraph_invoke` nodes — that is how the
quality-pattern recipes in §6 nest.

**Recursion guard.** An atomic depth counter caps nesting via `exec.max_depth`
(default 10); exceeding it is a non-recoverable error. See §7 for the
breadth-vs-depth caveat in wide compositions.

**Context inheritance** (`exec.context_inheritance`): `read_only` (default —
child reads the parent blackboard, writes don't leak), `snapshot` (child gets a
copy), or `isolated` (child sees nothing). Applies to `subgraph_invoke`, `map`,
and the loop family.

---

## 2. `subgraph_invoke`

Runs a named subgraph once, as a function call. The base primitive the other
three build on. Use it to factor a reusable sequence out of a graph.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `config.graph` | string | required | Name of the subgraph in the library |
| `exec.max_depth` | integer | `10` | Max concurrent invocation depth |
| `exec.context_inheritance` | `read_only` \| `snapshot` \| `isolated` | `read_only` | Blackboard inheritance mode |

#### outputs

The merged outputs of the subgraph's sink nodes (shape depends on the subgraph).
An empty subgraph or one whose sinks produce nothing returns an empty map.

```
%% @INVOKE handler: subgraph_invoke
%% @INVOKE config.graph: enrich_record
```

---

## 3. `map`

Data-driven fan-out: runs a subgraph once per element of a runtime list,
concurrently (capped) and order-preserved, then reduces the per-item results.
Where static `parallel:` fans out an author-time set of nodes, `map` fans out
over a list whose length is only known at runtime — psflow's declarative answer
to `items.map(run)`.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `config.over` | string | required | Input key holding the list (a `Vec`) to map over. One element → one invocation |
| `config.graph` | string | required | Subgraph run per element |
| `config.as` | string | `item` | Input key the element is bound to inside the subgraph |
| `config.max_concurrency` | integer | `16` | Max concurrent invocations |
| `config.reduce` | `collect` \| `quorum` | `collect` | How per-item results are reduced (see below) |
| `config.quorum.field` | string | — | (quorum) Boolean output field each item is checked for |
| `config.quorum.threshold` | integer | `1` | (quorum) Minimum `true` votes for `passed` |
| `config.on_item_error` | `skip` \| `fail` | `skip` | `skip` omits a failed item (counted in `errors`); `fail` fails the whole node |
| `exec.max_depth` | integer | `10` | Nesting guard for the map node |
| `exec.context_inheritance` | `read_only` \| `snapshot` \| `isolated` | `read_only` | Blackboard inheritance |

#### `reduce: collect` outputs

| Field | Type | Notes |
|---|---|---|
| `results` | array | Per-item output maps, ordered by input index |
| `count` | i64 | Number of successful items |
| `errors` | i64 | Number of failed items (always `0` when `on_item_error: fail`) |

#### `reduce: quorum` outputs

Counts how many items produced `true` in `quorum.field`. This is how adversarial
voting (§6) falls out of fan-out for free.

| Field | Type | Notes |
|---|---|---|
| `votes` | i64 | Items whose `quorum.field` was `true` |
| `passed` | bool | `votes >= quorum.threshold` |
| `count` | i64 | Number of successful items |
| `errors` | i64 | Number of failed items |

#### example — `map-demo`

```
graph TD
    GEN[Generate] --> FAN[Map Square]
    %% @GEN handler: rhai
    %% @GEN config.script: "#{ nums: [1, 2, 3, 4] }"
    %% @FAN handler: map
    %% @FAN config.over: nums
    %% @FAN config.graph: square_item
    %% @FAN config.as: item
```

The `square_item` subgraph reads `inputs.item` and returns `#{ squared: n * n }`;
the map node returns `results` = `[{squared:1},{squared:4},{squared:9},{squared:16}]`.

#### example — quorum vote

```
%% @VERIFY handler: map
%% @VERIFY config.over: findings
%% @VERIFY config.graph: judge_finding
%% @VERIFY config.reduce: quorum
%% @VERIFY config.quorum.field: real
%% @VERIFY config.quorum.threshold: 2
```

---

## 4. `loop`

Accumulating loop that generalizes `poll_until`. Each iteration runs a subgraph,
appends its produced items to a growing (optionally deduped) collection, and
injects that collection back into the next iteration as `state` — so a subgraph
can "find what it hasn't found yet." Termination is the first of `until`,
`until_dry`, or `max_iterations` to fire.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `config.graph` | string | required | Subgraph invoked per iteration |
| `config.collect` | string | — | Output key holding the per-round list of items. If unset, the whole output map is appended as one item per round |
| `config.until` | Rhai string | — | Stop when truthy. Scope: `state` (accumulated list), `iteration` (1-based i64), `output` (last round's output map) |
| `config.until_dry` | integer | — | Stop after this many consecutive rounds that add no new items |
| `config.dedup_key` | Rhai string | — | Expression over `item` returning a dedup key. Without it, items are deduped by canonical JSON |
| `config.max_iterations` | integer | required | Hard cap (`>= 1`). The backstop that always bounds the loop |
| `config.delay_ms` | integer | `0` | Delay between iterations (first fires immediately) |
| `config.state_as` | string | `state` | Input key the accumulated list is injected as |

#### outputs

| Field | Type | Notes |
|---|---|---|
| `collected` | array | The full accumulated (deduped) collection |
| `count` | i64 | `len(collected)` |
| `iterations` | i64 | Iterations performed |
| `dry_rounds` | i64 | Trailing consecutive rounds that added nothing |
| `stopped_by` | string | `until` \| `until_dry` \| `max_iterations` — branch on this |
| `output` | map | The last iteration's raw subgraph output |

Hitting `max_iterations` is not a failure; callers branch on `stopped_by`.

#### example — `loop-demo` (accumulate to a target)

```
graph TD
    LOOP[Accumulate]
    %% @LOOP handler: loop
    %% @LOOP config.graph: find_more
    %% @LOOP config.collect: items
    %% @LOOP config.until: "len(state) >= 6"
    %% @LOOP config.max_iterations: 10
```

`find_more` reads `inputs.state`, returns two ints derived from its length; the
loop stops the round after `collected` reaches 6, with `stopped_by = "until"`.

#### example — loop until dry, with dedup

```
%% @SWEEP handler: loop
%% @SWEEP config.graph: find_issues
%% @SWEEP config.collect: issues
%% @SWEEP config.dedup_key: "item.id"
%% @SWEEP config.until_dry: 2
%% @SWEEP config.max_iterations: 20
```

Stops after two consecutive rounds surface no new `item.id`.

---

## 5. `poll_until`

The minimal fixed-attempt case: invoke a subgraph on a fixed-delay loop until a
Rhai predicate over its output returns true, or a cap is hit. Reach for `loop`
instead when you need to accumulate across rounds; reach for `poll_until` when
you just need to wait for a condition.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `config.graph` | string | required | Subgraph invoked per attempt |
| `config.predicate` | Rhai string | required | Stop when truthy. Scope: `output` (subgraph output map), `attempt` (1-based i64) |
| `config.max_attempts` | integer | required | Hard cap (`>= 1`) |
| `config.delay_ms` | integer | required | Fixed delay between attempts. First attempt fires immediately |

#### outputs

| Field | Type | Notes |
|---|---|---|
| `attempts_used` | i64 | Attempts performed |
| `timed_out` | bool | `true` iff the cap was reached without a predicate match |
| `output` | map | Final subgraph output (the match, or the last attempt before the cap) |

Cap without a match is not a node failure; callers branch on `timed_out`.

```
%% @Poller handler: poll_until
%% @Poller config.graph: check_job_status
%% @Poller config.predicate: output.status == "complete"
%% @Poller config.max_attempts: 20
%% @Poller config.delay_ms: 3000
```

---

## 6. Quality-pattern recipes

The dynamic-orchestration patterns (fan-out, adversarial verify, loop-until-dry,
find→dedup→panel), expressed declaratively by composing the handlers above.
Subgraphs nest, so a `loop`'s subgraph can contain a `map`, whose subgraph can be
an LLM judge — durable, renderable, inspectable the whole way down.

### 6.1 Fan-out + adversarial verify

Run a panel of judges over each candidate and keep it only if a quorum agrees.
One `map` with `reduce: quorum` over a judge subgraph.

```
%% @VERIFY handler: map
%% @VERIFY config.over: candidates
%% @VERIFY config.graph: judge_candidate     %% each judge emits a bool `real`
%% @VERIFY config.reduce: quorum
%% @VERIFY config.quorum.field: real
%% @VERIFY config.quorum.threshold: 2
```

For *perspective-diverse* verification, make `judge_candidate` itself a `map`
over a list of lenses (correctness / security / repro), so each candidate is
judged from several angles before the quorum.

### 6.2 Loop-until-dry / accumulate-to-target

Keep generating until the well runs dry, or until you hit a target count. One
`loop`:

- accumulate-to-target → `config.until: "len(state) >= N"`
- loop-until-dry → `config.until_dry: 2` (two empty rounds = done)

`max_iterations` is the mandatory backstop in both cases.

### 6.3 Find → dedup → panel (composed)

The exhaustive-review shape: each round finds candidates, dedups against
everything seen, runs a verification panel, and repeats until dry. A `loop`
(carrying the dedup via `dedup_key`) whose subgraph runs a `map` verify step
each round.

```
%% loop carries dedup + termination
%% @REVIEW handler: loop
%% @REVIEW config.graph: find_and_verify_round
%% @REVIEW config.collect: confirmed
%% @REVIEW config.dedup_key: "item.file + \":\" + item.line"
%% @REVIEW config.until_dry: 2
%% @REVIEW config.max_iterations: 15
```

Inside `find_and_verify_round`: a finder node produces candidates, then a `map`
with `reduce: quorum` (recipe 6.1) keeps only the confirmed ones, which the loop
accumulates. Dedup is against the accumulated `state`, not the per-round output,
so a candidate rejected once never re-enters.

---

## 7. Known limitations

- **`map.reduce`** supports only `collect` and `quorum`. A custom reduction or
  accumulate-to-blackboard mode is not yet available — collect and reduce in a
  downstream node for now.
- **`loop`** supports `until` (stop-when-true) but not `while`
  (continue-while-true). Invert the predicate, or stop via `until_dry` /
  `max_iterations`.
- **Depth guard counts breadth.** The recursion guard's counter is shared across
  concurrently-running siblings, so a composition that is both deeply nested
  **and** wide (a `map` of `map`s) can trip `max_depth` on breadth rather than
  true recursion depth. Raise `exec.max_depth` if a legitimately wide
  composition hits a spurious depth error.

---

## 8. Running

Any `.mmd` in the graphs dir is automatically loaded as a named subgraph (its
file stem is the library name), so a composition handler can reference it with no
registration step. Run a top-level graph with:

```
just graph <graph-name> [--input key=value ...]
```

(or `cargo run --bin psflow-run --features runtime -- <graph-name>`). The
accumulated `state` (loop) and per-element binding (map) arrive as inputs the
subgraph reads via `inputs.<key>`.

---

## 9. Related

- Leaf-handler config surface: [mermaid-annotation-reference.md](./mermaid-annotation-reference.md)
- Authoring guide: [writing-flow-mmd.md](./writing-flow-mmd.md)
- Handler sources: `src/handlers/{subgraph_invoke,map,loop_handler,poll_until}.rs`
- Example graphs: `map-demo` + `square_item`, `loop-demo` + `find_more`
