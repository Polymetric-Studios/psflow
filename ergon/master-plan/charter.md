# Charter

Last-Reviewed: 2026-06-20

## §1 Problem

A workflow's definition normally fragments across four sources that drift apart — the diagram, the type schema, the node implementation, and the execution config — and graph engines are domain-bound (game behavior trees, ML pipelines, CI DAGs) with one execution paradigm baked in. psflow collapses topology, typing, node implementation, and execution semantics into a **single annotated-Mermaid `.mmd` file**, and makes the execution paradigm a **swappable executor**, so one graph model serves pipelines, behavior trees, dataflow, and reactive networks — embeddable in native, WASM, and Python hosts.

## §2 Audience

Solo developer (Polymetric Studios). The engine is a reusable library; its immediate consumer is **personal LLM-automation** via psflow-run (Composio-triggered flows over Google Sheets etc., scheduled named-graphs, PTY-driven `claude`). Secondary targets: native game embedding (C-FFI/Unity), the WASM debugger, and PyO3.

## §3 Non-goals

- Not a visual graph editor — the `.mmd` is authored as text (and still renders as a diagram); the debugger only observes.
- Not a domain framework (game-only, ML-only) — the model is domain-agnostic by construction.
- Not a distributed / clustered orchestrator — single-process execution.
- Not bound to one LLM vendor — access is the `AiAdapter` trait.
- Not a general scripting language — Rhai fills the in-graph scripting role.

## §4 North-star metrics

- Round-trip fidelity: `.mmd` → `Graph` → `.mmd` is structurally equivalent.
- One `.mmd` runs unchanged across executor strategies (topological / reactive / stepped / event-driven).
- A new provider, handler, or integration is added without editing the engine core.
- The `graph`+`mermaid` core builds to WASM and embeds in a host without forking.

## §5 In-scope surfaces

- `src/graph/**`, `src/mermaid/**`, `src/error.rs` — the portable core (no async stack).
- `src/execute/**`, `src/adapter/**`, `src/auth/**`, `src/handlers/**`, `src/scripting/**`, `src/{registry,template,validation,blackboard,debug_server}*` — the runtime engine.
- `src/bin/{psflow_run,composio,manifest}.rs`, `src/main.rs` — the app / runner tier.
- `crates/psflow-wasm/**`, `debugger/**` — WASM build + debugger UI.
- `examples/*.mmd`, `graphs/**` — examples and named-graphs.

## §6 Out-of-scope

- Distributed execution, clustering, multi-host scheduling.
- A GUI graph builder.
- Provider-specific SDK lock-in.
- A database / persistence layer beyond run-records, cross-run-state, and execution-snapshots (flat files).

## §7 Boundary rules

- Engine code is provider-neutral and stays out of `src/bin/`; the app tier depends on the engine, never the reverse.
- Integration-handlers (`composio`, `claude_workflow`) register only via psflow-run's `register_integrations`.
- `graph` and `mermaid` never pull the async runtime (the `runtime` feature line is the boundary).
- Secrets resolve through a `SecretResolver` (logical-names), never inlined in a graph.
