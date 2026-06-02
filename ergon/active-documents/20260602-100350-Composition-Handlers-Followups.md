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

The composition handlers are implemented, unit-tested, and wired into `psflow-run`, verified end-to-end (`map-demo` → squares; `loop-demo` → accumulates to a target). The reference doc (task 9.1.1) is written. The remaining two items (`claude_workflow` handler, `map`/`loop` hardening) are not started. Nothing blocks them.

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

The remaining expressiveness frontier is truly-arbitrary imperative orchestration that doesn't fit `map`/`loop`. The proposed shape: a `claude_workflow` handler that invokes a Claude Code dynamic workflow (bundled like `/deep-research`, or a saved one) as a single psflow node — psflow owns the durable/triggered/scheduled outside, the agent swarm runs inside, and the node returns the workflow's final result as structured outputs.

#### 4.3.0 Chosen approach — PTY-driven interactive session (supersedes `-p`)

The build direction pivoted from one-shot `claude -p` to **driving the real interactive `claude` TUI headless over a pseudo-terminal**. Rationale: (a) it exposes the full interactive surface (slash commands, `/workflows`, approval dialogs, pause/resume) — the whole point of the feature; (b) a genuine TTY session bills as *interactive* (normal plan limits), not from the separate Agent SDK credit pool that both `-p` and the Agent SDK draw from after 2026-06-15 — PTY-driving is the only way to automate Claude Code while staying in the cheaper bucket. The `-p` analysis below (§4.3.1–4.3.4) stays valid as a documented fallback.

Architecture (implemented as Layer 1 — see §11.1): a `portable-pty` master/slave hosts `claude` (real TTY → interactive UI); a reader thread pumps output into a `vt100` virtual screen; a *recognizer* detects input-ready and turn-complete (the latter keys off the live spinner `…` line, since the `❯` box never disappears). Results are read **deterministically from the session transcript** Claude Code writes (`~/.claude/projects/<project>/<session-id>.jsonl`) — the session-id is pinned via `--session-id` at spawn so the path is known; the last `assistant` message is the payload. Screen-scrape is a fallback only. The earlier "ask the model to write a result file" idea was rejected as non-deterministic (model-compliance-dependent). Lives in `src/adapter/claude_terminal.rs` behind a `terminal` cargo feature (`portable-pty`/`vt100`/`uuid`). Proven end-to-end driving the real v2.1.160 TUI (`source=Transcript`).

Determinism boundary: transcript reading makes result *retrieval* deterministic; the answer *content* is still model output. A machine-*enforced* schema only exists on the rejected `-p --json-schema` path — a real tradeoff of driving the human UI.

Remaining layers: Layer 2 = a `ratatui` supervision TUI (observe/approve a psflow-driven session; maps to psflow's `supervised` mode); then wire the `claude_workflow` handler onto the session (cancellation token → Esc/Ctrl-C; approval-dialog recognizer). Open hardening: approval-dialog detection/answering, turn-complete robustness across `claude` versions (contained to the recognizer), concurrency (one child process per session).

#### 4.3.1 Feasibility — resolved (headless `claude -p` is the path; kept as fallback)

The gating §6 question is answered by the official docs (workflows page + headless/Agent-SDK page, June 2026) and confirmed against the local CLI (v2.1.160 ≥ the v2.1.154 minimum; `--output-format`, `--json-schema`, `--bare`, `--mcp-config`, `--strict-mcp-config`, `--permission-mode` all present):

- Dynamic workflows are officially supported in non-interactive `claude -p` and the Agent SDK — not interactive-only. So the handler is buildable as designed.
- In `claude -p` / Agent SDK the launch approval is auto ("the run starts immediately"); spawned agents run in `acceptEdits` and follow the configured permission rules with no interactive prompt.
- Recommended shape for a Rust node: shell out (mirroring the existing `shell` handler) to `claude -p --bare --output-format json --json-schema <node-output-schema> --permission-mode <acceptEdits|dontAsk> --mcp-config <service-cred MCP>`, with the prompt containing the word `workflow`. Parse `.structured_output` (schema path) or `.result` (text) into the node's typed outputs.
- Auth: `ANTHROPIC_API_KEY` (bare mode skips OAuth/keychain). Note billing change effective ~2026-06-15: `claude -p` + Agent SDK usage on subscription plans draws from a separate Agent SDK credit pool; API-key auth bills normally.
- Caps: 16 concurrent agents / 1000 total per run.

#### 4.3.2 Spike results (run 2026-06-02, local CLI v2.1.160)

Two cheap `claude -p` runs (a 1-agent "return 42" workflow) settled the core questions:

- Workflow actually fires under `-p`: CONFIRMED. With the word `workflow` in the prompt, `stream-json --verbose` shows a real `Workflow` tool_use plus the background lifecycle (`system/task_started` → `task_progress` → `task_notification` → `task_updated`) — not Claude answering directly.
- Blocking + final result: CONFIRMED. `claude -p` blocks through the background run to completion and returns `result/success` (exit 0, `is_error:false`). Wall time ~14–20s for the trivial case.
- Structured output via `--json-schema`: CONFIRMED. The plain `result` is prose with the value embedded ("…returned {\"answer\": 42}"), but `.structured_output` is clean machine JSON (`{"answer":42}`). The handler must use `--json-schema` + read `.structured_output`; do not scrape `.result`. The JSON envelope also carries `session_id`, `usage`, `modelUsage`, `total_cost_usd`, `permission_denials`, `is_error` — usable for outputs, cost, and error detection.
- Cost: ~$0.32 (stream-json run) and ~$0.65 (json + schema, one extra coercion turn) for trivial 1-agent runs. Workflows carry meaningful fixed per-call overhead; the handler should expose model/concurrency controls and callers should size accordingly.

Still open (needs an interactive save, not blocked on cost):

- Determinism / run-by-name: the keyword path has Claude write a fresh script each run (deterministic *intent*, non-deterministic *script*). Running a saved workflow verbatim by name (`.claude/workflows/<name>` → `/<name>`) under `-p` is untested — no saved workflows exist locally, and saving needs interactive `/workflows` → `s`. Headless docs warn slash-commands/skills are interactive-only, so this may not work in `-p` at all. The proven keyword path is sufficient for v1 of the handler (it builds its own prompt); promote run-by-name only if a caller needs byte-identical orchestration across runs.

#### 4.3.3 Reuse the existing `-p` plumbing

psflow already shells out to `claude -p` in exactly one place — `ClaudeCliAdapter` (`src/adapter/claude_cli.rs`), the primary dev AI adapter behind `llm_call` nodes. Its `tokio::process::Command` build, stdout capture, JSON parse, `session_id`/`usage` extraction, and stderr/exit-status error handling are a ready template for `claude_workflow`; don't rebuild them. Three deltas the workflow handler must add over the adapter:

- Structured outputs: the adapter parses JSON out of the `.result` text (fragile). The handler should pass `--json-schema` and read `.structured_output` (the documented field).
- Deterministic/scheduled runs: the adapter passes no `--bare` / `--mcp-config`, so headless runs absorb ambient local hooks/MCP/CLAUDE.md. The handler should add `--bare` + explicit `--mcp-config`.
- Inner-agent permissions: the adapter sets no permission flags. The handler should set `--permission-mode` / `--allowedTools` for the agents the inner workflow spawns.

#### 4.3.4 MCP caveat

Interactively-authenticated MCP servers (claude.ai connectors) are absent in headless runs. Any MCP the inner workflow needs (e.g. ergon) must authenticate via env/service credentials and be passed with `--mcp-config` (optionally `--strict-mcp-config`).

## 5. Decisions

- Encapsulate dynamism in handlers, keep the graph declarative. Rationale: preserves psflow's renderable/inspectable/durable graph while gaining runtime-dynamic behavior. Rejected alternative: going imperative (a JS-style script), which is what Claude workflows do and what sacrifices the visual graph.
- Compose from `subgraph_invoke`, no engine changes. Rationale: `map`/`loop` are leaf handlers over existing machinery; lower risk, no executor surface area. Rejected alternative: a new dynamic-fan-out executor.
- Voting is `map` + `reduce: quorum`, not a separate primitive. Rationale: adversarial-verify falls out of fan-out for free.
- `loop` generalizes `poll_until` rather than replacing it. Rationale: `poll_until` stays the minimal fixed-attempt case; `loop` adds accumulation and flexible termination.

## 6. Open questions

- [x] Is a Claude Code dynamic workflow invocable from a standalone process (headless `claude -p` or the Agent SDK) so a `claude_workflow` handler can call it, or is it interactive-session-only? **Resolved: YES** — officially supported in `claude -p` and the Agent SDK (research preview, v2.1.154+; local CLI v2.1.160 confirmed). Buildable as designed via a `claude -p --output-format json --json-schema …` subprocess. Two sub-questions deferred to an empirical spike (§4.3.2): saved-workflow-run-by-name non-interactively, and whether `-p` blocks-and-returns the workflow's final result.
- [ ] How should `map`'s reducers extend — a named `ResultReducer` registry, an accumulate-to-blackboard mode, or both? Resolving this sets the `reduce` config surface before more callers depend on `collect`/`quorum`.

## 7. Usage

Author a subgraph as any `.mmd` in the graphs dir (its file stem is its library name). Reference it from another graph via the `map`/`loop`/`poll_until`/`subgraph_invoke` handler annotations (config keys in §4.1). Run with `psflow-run <graph>` or `just graph <graph>`. The accumulated `state` (loop) and per-element binding (map) arrive as node inputs the subgraph reads. Recursion and mutual reference work because the registry is finalized before the deferred slots are set.

## 8. Known limitations

- `map.reduce` supports only `collect` and `quorum`. Promote when a workflow needs a custom reduction (named `ResultReducer`) or accumulate-to-blackboard.
- `loop` supports `until` (stop-when-true) but not `while` (continue-while-true). Promote when a graph reads more naturally as a while-loop.
- The `DepthGuard` counts nesting but is shared across concurrently-running siblings, so deeply nested **and** wide `map`/`loop` compositions can trip `max_depth` on breadth rather than true recursion depth. Promote when a real nested-concurrent composition hits a spurious depth error; the fix is to distinguish breadth from depth (e.g., per-branch depth context).

## 9. Plan

### 9.1 Planned tasks

- [x] Write the composition reference doc covering `map` / `loop` / `poll_until` / `subgraph_invoke`: per-handler config surface and outputs (from §4.1) plus the quality-pattern recipes (§4.2). Place under `docs/` alongside the mermaid annotation reference. → `docs/composition-handlers-reference.md`; cross-linked from `mermaid-annotation-reference.md`.
- [x] Resolve the §6 feasibility question for `claude_workflow` (headless / Agent SDK invocation). **Done** — resolved to headless `claude -p` (see §4.3.1). Remaining for this item:
  - [x] Empirical spike (§4.3.2): confirmed the `workflow` keyword fires a real workflow under `-p`, that `-p` blocks and returns the final result, and that `--json-schema` yields clean `.structured_output`. Run-by-name left untested (needs an interactive save; keyword path is sufficient for v1).
  - [x] Build direction chosen: PTY-driven interactive session (§4.3.0), superseding the `-p` subprocess plan.
  - [x] Layer 1 — `ClaudeTerminalSession` engine (`src/adapter/claude_terminal.rs`, `terminal` feature): spawn/wait_ready/submit/wait_turn/send_key/interrupt/run_collecting_result; vt100 screen + cursor-on-`❯` recognizer; file-primary, scrape-fallback results. 6 unit tests; proven end-to-end against the real v2.1.160 TUI.
  - [x] Wire the `claude_workflow` handler onto the session. `src/handlers/claude_workflow.rs` (`terminal` feature): config `prompt` (templated) / `output` / `model` / `permission_mode` (default `acceptEdits`) / `approval` (allow|deny, default allow) / `timeout_ms` / `cwd` / `args`; runs the session on `spawn_blocking`, maps the node cancellation token onto the session cancel flag, returns `{<output>, source, session_id}`. Registered in `psflow-run` `build_handlers` (gated). Applies Phase D (non-prompting mode + AllowAll backstop). Verified end-to-end: `graphs/claude-demo.mmd` → `answer="42"`, `source="transcript"`. Run with `cargo run --features terminal --bin psflow-run -- <graph>` (the `terminal` feature is off by default).
  - [~] Layer 2 — human-facing `ratatui` supervision TUI. DEFERRED ("later, maybe"). Terminology note: the work so far *drives Claude Code's own TUI* over a PTY — that is not a psflow UI. A separate human-in-the-loop supervision UI is only worth building if a real need appears; headless runs are fully covered by `ApprovalPolicy`.
  - [x] Harden turn-complete detection. Captured real TUI states (`examples/pty_capture.rs`) and found the fragility: the `❯` input box stays visible the entire turn, so `input_ready` can't detect completion. Fixed: completion now keys off the live spinner status line (`✽ Crunching… ` — spinner glyph + trailing `…`) disappearing (`!busy`), distinct from the completed summary (`✻ Brewed for 3s`, no `…`). `is_busy` recognizer with glyph + ellipsis + short-line guards; `input_ready` kept in the predicate to avoid false-complete while a modal has focus. 5 new unit tests (11 total); confirmed end-to-end.
  - [x] Approval-dialog Phase A (capture) + Phase B (detect + bug fix). Captured a real dialog by forcing a Write-tool prompt in `--permission-mode default` (saved fixture `src/adapter/testdata/approval_dialog.txt`). It revealed a correctness bug: the dialog's selection cursor reuses the `❯` glyph, so `input_ready` reads true on a dialog and `wait_turn` was false-completing on it. Fixed `wait_turn` (added `!approval` guard) and built `detect_approval` (`ApprovalPrompt{question,options,selected}`; structural anchor = numbered list with a `❯`-highlighted option, which excludes numbered content lists). 5 new tests incl. the real fixture (19 total).
  - [x] Approval-dialog Phase C (answer + policy). `ApprovalChoice` (Allow/Deny/Select), `ApprovalPolicy` (AllowAll/DenyAll/`custom(closure)` — covers allowlist and the TUI's Ask), `answer_approval`, and the approval-aware `drive_to_completion` loop (detect stable dialog → policy decide → send key → wait-cleared → resume) now backing `run_turn`. Verified live: `--permission-mode default` + AllowAll auto-approved a Write-tool dialog, the file was actually created, turn completed, result from transcript. 22 unit tests.
  - [ ] Phase D (defaults, a handler-wiring decision): headless `claude_workflow` launches in a non-prompting permission mode (dialogs never appear) with an explicit `ApprovalPolicy` backstop; supervised mode sets `ApprovalPolicy::custom` routed to the TUI. Apply when wiring the handler.
  - [ ] Per-session concurrency model (one child process + PTY per session).
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
- Composition reference doc — `docs/composition-handlers-reference.md` (shared subgraph model, per-handler config/outputs for all four, quality-pattern recipes, known limitations); cross-linked from `mermaid-annotation-reference.md`.
- `claude_workflow` Layer 1 — `ClaudeTerminalSession` engine (`src/adapter/claude_terminal.rs`, `terminal` feature; `portable-pty` + `vt100` + `uuid`): drives the real interactive `claude` TUI headless over a PTY. spawn/wait_ready/submit/wait_turn/send_key/interrupt/run_turn. Proven end-to-end (`cargo run --example pty_spike --features terminal`).
- Recognizer hardening — `is_busy` spinner-line detector (live `…` status vs completed summary); `wait_turn` keys off `!busy` (the `❯` box is always present, so `input_ready` alone can't detect completion). Capture tool `examples/pty_capture.rs` for fixturing TUI states. Approval-dialog handling deferred (see §9.1).
- Deterministic results — `run_turn` reads the final assistant message from the session transcript JSONL Claude Code writes, with the session-id pinned via `--session-id` so the path is known (`find_transcript`/`last_assistant_text`); screen-scrape demoted to fallback. Replaced the rejected model-writes-a-file approach.
- Approval-dialog detection — captured a real dialog (fixture `src/adapter/testdata/approval_dialog.txt`); `detect_approval` parses it (numbered list with a `❯`-highlighted option). Fixed a `wait_turn` correctness bug it exposed (the dialog's `❯` cursor collided with the input-box marker, causing false turn-completion).
- Approval answering + policy — `ApprovalChoice`/`ApprovalPolicy` (AllowAll/DenyAll/`custom`), `answer_approval`, and the approval-aware `drive_to_completion` loop backing `run_turn`. Verified live (`cargo run --example pty_approve --features terminal`): forced dialog auto-approved by AllowAll, action ran, result from transcript.
- Transcript-based turn-completion — `drive_to_completion` now detects turn end deterministically from the transcript (`count_end_turns`: a new `assistant` entry with `stop_reason=="end_turn"` past a per-turn baseline), not the screen spinner. Spike (`examples/pty_transcript.rs`) showed it fires at the same tick as the spinner, is multi-tool-safe (skips intermediate `tool_use`), and won't fire during an approval pause. Screen is now used only for dialog detection. Added a `cancel_flag` (`cancel_handle()`/`set_cancel_flag()`) so the blocking drive loop honors external cancellation. 23 unit tests.
- `claude_workflow` handler — `src/handlers/claude_workflow.rs` (`terminal` feature): a psflow node that runs a prompt/workflow in a real interactive `claude` session over the PTY driver and returns the transcript result as typed outputs (`<output>`/`source`/`session_id`). `spawn_blocking` + node-cancellation → session cancel flag; Phase D defaults (acceptEdits + AllowAll). Registered in `psflow-run`. Verified end-to-end through a graph (`graphs/claude-demo.mmd` → `answer="42"`, `source="transcript"`). 3 handler unit tests; lib + bins + examples build warning-clean with `terminal`.

### 11.2 Deferred / superseded

_None._

## 12. Appendix

_None._

## 13. Related

- Handler sources: the `map`, `loop_handler`, `poll_until`, and `subgraph_invoke` modules under `src/handlers/`.
- Runner wiring: `build_handlers` / `load_graph_library` in the `psflow-run` binary.
- Example graphs: `map-demo` + `square_item`, `loop-demo` + `find_more` in the graphs dir.
- Claude Code dynamic workflows (the comparison that motivated this work): https://code.claude.com/docs/en/workflows
