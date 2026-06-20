# Architecture

Last-Reviewed: 2026-06-20

## 1. Preamble

### 1.2 Purpose

The curated architecture narrative — the system's shape and intent (the *why* a generated import graph cannot supply). The primer links here for orientation. Per-file structure and the literal import graph are the generated views (§13), not this file.

## 2. Scope

### 2.1 In scope

The two halves of psflow: the **provider-neutral engine** (the `psflow` library crate) and **psflow-run** (the personal-automation runner, `src/bin/psflow_run.rs`). Layers, the feature-gated portability seam, allowed dependency directions, and the async runtime model.

### 2.2 Out of scope

Per-file structure and the literal import graph — the generated code views (`psflow-manifest` / `generate_*`), linked in §13. The annotated-Mermaid wire syntax — `terminology.md` and `reference/`.

## 4. Body

### 4.1 Layers

psflow is **one graph data model with swappable executor strategies**, split into two compile tiers by the `runtime` Cargo feature.

**Core (no `runtime` feature — compiles without the async stack tokio/reqwest/rhai; WASM- and embed-portable):**

- **`graph/`** — the immutable topology and type system: `Graph` (backed by `petgraph::StableDiGraph`), `Node`/`NodeId`, `EdgeData`, `Port`, `PortType`/`Value`, `GraphMetadata`, `Subgraph`/`SubgraphDirective`/`SubgraphTopology`, graph + DAG validation, and graph-local auth declarations (`auth_decl`). Custom serde flattens the petgraph to portable `{metadata, nodes[], edges[], subgraphs[]}` JSON.
- **`mermaid/`** — the annotated-Mermaid format boundary: `parse` (line parser → `ParsedMermaid`, purely syntactic) → `annotation` (`%% @NodeID key.path: value` → nested JSON, reserved-key validation) → `loader` (`load_mermaid()`: parse + annotate + resolve ports/subgraph-directives → executable `Graph`) → `export` (`export_mermaid()`: `Graph` → annotated `.mmd`, round-trip stable).
- **`error/`** — `GraphError` (structural) and `NodeError` (runtime) hierarchies.

**Runtime (`runtime` feature — tokio/reqwest/rhai):**

- **`execute/`** — the swappable executors and run state. Executors walking the same `Graph` via the `Executor` trait: `topological` (dependency-ordered waves), `reactive` (fire-on-input-ready dataflow), `stepped` (one tick per call, behavior-tree), `event_driven` (external events push into entry nodes). Plus `blackboard` (scoped state), `context` (`ExecutionContext`), `control` (guard eval; sequence/parallel/race/loop strategies), `concurrency`, `retry`/`BackoffStrategy`, `snapshot` (checkpoint-resume), `trace` (per-node timing/retry, ancestor-scoped), `lifecycle`, `loop_controller`, and the `event`/`event_bus` (`tokio::broadcast`) observability channel.
- **`adapter/`** — the AI abstraction. `AiAdapter` trait (`complete`/`judge`/`capabilities`) + `AdapterRegistry`; backends `MockAdapter`, `ClaudeCliAdapter`, `AnthropicApiAdapter` (prompt-cache + structured outputs), `OpenAiCompatAdapter` (any OpenAI-wire provider — OpenRouter/OpenAI/Groq/Together/local), and the `terminal`-gated `claude_terminal` (PTY-driven real claude TUI). Stateless — continuity rides on ancestor-scoped `ConversationHistory` on the blackboard; arbitrary-model via per-node `config.model`.
- **`auth/`** — graph-local credential injection: named `auth-strategy`s (`static_header`, `bearer`, `cookie_jar`, `hmac`), a host-implemented `SecretResolver` mapping secret-roles → logical-names, and secret zeroization (`zeroize`). Applied to HTTP/WS handlers via the execution context.
- **`handlers/`** — node implementations registered by name. Built-ins (`passthrough`/`transform`/`gate`/`merge`/`split`/`delay`/`log`/`error_transform`, `http`/`websocket`, `file_io`, `llm_call`, `rhai`, `accumulator`, `human_input`, `subgraph_invoke`, `map`, `loop_handler`/`poll_until`, `shell`, `json_transform`) plus **isolated integration handlers** (`composio`, `claude_workflow`) registered only by psflow-run.
- **`scripting/`** — sandboxed Rhai: `engine` (execution limits, cooperative cancel, `ctx_*` access) + `bridge` (`Value` ↔ Rhai `Dynamic`).
- **`registry`**, **`template`** (`{var}`/`{inputs.*}`/`{ctx.*}` + `{#if}` interpolation), **`blackboard`** helpers, **`debug_server`** (the WASM debugger's backend), **`validation`**.

**App tier (binaries):** `psflow-run` (the personal runner — named-graphs, runtime-inputs, cross-run-state, scheduling, tool-response-cache, run-records, listen-mode, on-failure-hook; registers integration-handlers via `register_integrations`), `composio` (Composio trigger/SDK CLI), `psflow-manifest` (code-view generator), `psflow` (default CLI stub). The `crates/psflow-wasm` workspace member compiles the core to WASM (`wasm-pack`) for the `debugger/` (vite) UI.

### 4.2 Boundaries and allowed dependency directions

The load-bearing rule (the Pycnocline): **references run up-density only** — toward the lighter, more-agnostic layer.

- **`graph` + `mermaid` are the lightest, most-portable layer** and compile without the async stack (DR-003). They depend on nothing above; everything depends down onto them. Independently usable for parse/build/validate/export without pulling tokio.
- **The engine never depends on psflow-run or on integration-handlers** (DR-004). `composio` and `claude_workflow` live under `handlers/` but are registered only by the app (`register_integrations`), so removing them never touches the core — the cardinal isolation boundary.
- **Adapters and handlers depend on `graph`/`execute` traits, never the reverse.** A new provider (adapter) or node type (handler) is added without modifying the executor or the data model.

A down-density reference — the engine reaching into psflow-run, or `graph` pulling a runtime dep — is the cardinal structural violation.

### 4.3 Process / runtime model

Async on tokio. An executor walks the `Graph` and runs node handlers; a `SubgraphDirective` (Parallel/Race/Event/Loop/Named) on a subgraph delegates to the matching `control` strategy. Concurrency is capped globally / per-parallel / per-adapter. The `event_bus` (`tokio::broadcast`) emits execution events for live tracing and the debug server. Runs are resumable via `ExecutionSnapshot` (checkpoint-resume). psflow-run layers per-graph `cross-run-state`, `run-record`s, `tool-response-cache`, `listen-mode`, and the `on-failure-hook` on top of a single engine run.

## 5. Decisions

The load-bearing decisions live in `decisions.md` (DR-001…DR-007): the annotated-Mermaid single-file format, petgraph backing + portable serde, the feature-gated core/runtime split, the engine-vs-runner isolation boundary, the AiAdapter abstraction, swappable executors over one model, and all-errors validation + structural typing.

## 13. Related

- `psflow-manifest` (`src/bin/manifest.rs`) / `generate_code_manifest` — per-file structure and exports (mechanical view; a cross-check, not a replacement for this narrative).
- `generate_architecture_map` — the static import graph (import-reality cross-check).
- `terminology.md` — the vocabulary used here.
- `reference/` — annotated-Mermaid format and external protocol specs (when present).
