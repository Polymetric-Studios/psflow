# Decisions

Last-Reviewed: 2026-06-20

<!-- Append-only log of project-plan-level decisions (DR-NNN), newest at top.
Sequential, never renumbered. This is a read-only rationale archive: a DR records
the *why* and is read for context — citing it is optional, nothing back-links it,
and DR numbers stay out of code (see document-schemas.md §1.7).
DR-001..DR-007 were seeded from the project's existing architecture (legacy
project-data/ + the live source); their dates are the documented engine-era
anchor (2026-03-28, "Phase 3") where the exact decision predates this journal. -->

## DR-007: Validation collects all errors; structural type compatibility

**Date:** 2026-03-28
**Status:** accepted

### Context
A graph author needs every structural problem in one pass, and ports must type-check across loosely-related domains without a nominal type registry.

### Decision
`validate()` / `validate_as_dag()` return `Vec<GraphError>` rather than short-circuiting on the first failure (orphans, missing ports, type mismatches, self-loops, cycles via Tarjan SCC). `PortType` compatibility is **structural**: exact match, `Any` wildcard, `i64`→`f32` coercion, recursive `Vec`/`Map` element compatibility; `Domain(name)` types match by name only.

### Consequences
- One validation pass surfaces all errors — fewer edit/re-run cycles.
- Structural typing keeps the model domain-agnostic; domain types cost only a name.
- Name-only domain matching is permissive — a domain mismatch is caught by name, not shape.

## DR-006: Swappable executor strategies over one graph model

**Date:** 2026-03-28
**Status:** accepted

### Context
Pipelines, behavior trees, dataflow, and reactive networks are usually four separate engines with four data models. The bet: one model can serve all four if the *walk* is swappable.

### Decision
The `Graph` is paradigm-neutral; execution is a runtime-selected `Executor` trait object — `topological` (dependency waves), `reactive` (fire-on-input-ready dataflow), `stepped` (behavior-tree tick), `event_driven` (external push). `@graph default_executor` is a hint, not enforced at load.

### Consequences
- The same `.mmd` can run under different paradigms without edits.
- New paradigms are added as executors without touching the data model or handlers.
- The model must stay executor-agnostic — no executor-specific assumptions leak into `graph/`.

## DR-005: AiAdapter trait — swappable, stateless LLM backends

**Date:** 2026-03-28
**Status:** accepted

### Context
The graph needs LLM intelligence without binding to one vendor, and must reuse conversational context across nodes.

### Decision
LLM access is the `AiAdapter` trait (`complete`/`judge`/`capabilities`), resolved by name from an `AdapterRegistry`. Adapters are **stateless**; continuity rides on ancestor-scoped `ConversationHistory` on the blackboard; arbitrary-model access comes from per-node `config.model`. Backends: `mock`, `claude_cli`, `anthropic_api`, `openai_compat`, and `terminal`-gated `claude_terminal`.

### Consequences
- A new provider is one adapter; graphs are unchanged.
- Statelessness makes runs reproducible and adapters trivially poolable.
- Conversation correctness depends on ancestor-scoping so parallel branches don't cross-contaminate.

## DR-004: Provider-neutral engine vs psflow-run app; integration-handlers isolated

**Date:** 2026-06-01
**Status:** accepted

### Context
Personal automation needs third-party integrations (Composio, a real claude TUI), but the engine must stay a clean, reusable, domain-agnostic platform.

### Decision
Split the system: the **engine** (the `psflow` library, provider-neutral) and **psflow-run** (the personal runner). Integration-handlers (`composio`, `claude_workflow`) are registered **only by psflow-run** (`register_integrations`), never by the engine's default registry. The engine never depends on psflow-run or on any integration.

### Consequences
- Integrations can be removed without touching the core (the cardinal isolation boundary).
- The engine stays reusable across hosts (native/WASM/PyO3).
- Integration handlers physically live under `handlers/` but are isolated by *registration*, not directory — a convention the registry must keep honoring.

## DR-003: Feature-gated core vs runtime

**Date:** 2026-03-28
**Status:** accepted

### Context
The data model + format layer should embed anywhere (WASM debugger, minimal hosts) without dragging in tokio/reqwest/rhai.

### Decision
`error`, `graph`, and `mermaid` compile with **no `runtime` feature** — no async stack. The execution engine (`execute`, `adapter`, `auth`, `handlers`, `scripting`, `registry`, `template`, `validation`, `debug_server`) is gated behind `runtime` (default-on). `terminal` separately gates the PTY claude backend.

### Consequences
- The lightest layer is independently usable and WASM-portable (the `psflow-wasm` crate builds it).
- The Pycnocline is enforced by the compiler, not just convention — a runtime dep cannot leak into `graph`/`mermaid`.
- Always-on crypto/schema deps (`hmac`/`sha2`/`zeroize`/`jsonschema`/`async-trait`) remain compiled even in core; only the heavy async stack is gated.

## DR-002: petgraph StableDiGraph backing + portable flattened serde

**Date:** 2026-03-28
**Status:** accepted

### Context
Incremental graph editing needs indices that survive node/edge removal, and the on-disk form must be portable and re-importable.

### Decision
Back `Graph` with `petgraph::StableDiGraph<Node, EdgeData>` + a `HashMap<NodeId, NodeIndex>` for O(1) string-ID lookup. Implement custom `Serialize`/`Deserialize` that flatten to portable `{metadata, nodes[], edges[], subgraphs[]}` JSON and rebuild the petgraph + node map on load.

### Consequences
- Stable indices across edits enable incremental editing and the debugger.
- The serialized form is tool-agnostic and round-trips.
- Custom serde is hand-maintained — new `Graph` fields must be threaded through both directions.

## DR-001: Annotated-Mermaid single-file definition format

**Date:** 2026-03-28
**Status:** accepted

### Context
Topology, typing, node implementation, and execution semantics usually live in four separate sources (diagram, schema, code, config) that drift apart.

### Decision
Collapse all four into one standard Mermaid `.mmd`. Node/graph configuration is embedded as structured `%%` comment annotations (`%% @NodeID key.path: value`, `%% @graph key: value`) that real Mermaid renderers ignore. `load_mermaid`/`export_mermaid` round-trip: export → re-import is structurally equivalent.

### Consequences
- One file is the single source of truth; it still renders as a normal diagram.
- No drift between the picture and the runnable definition.
- The annotation grammar is now a compatibility surface — reserved keys and dot-path expansion must stay stable for round-trip.
