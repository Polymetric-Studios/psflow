# Argus domain brief

Last-Reviewed: 2026-06-20

This standing brief tells Argus how to judge this project's coherence. It is the project-specific judgment context for the seam manifest (`coherence-manifest.json`); the agnostic Argus skill/agent stays free of it.

## 1. What the components are

The **engine** (the `psflow` library, provider-neutral) plus the **app tier** on top.

- **graph** — the immutable data model + type system (petgraph-backed). The lightest, most-agnostic layer; compiles with no async stack.
- **mermaid** — the annotated-`.mmd` format boundary: parse → annotate → load / export (round-trip). Also no async stack.
- **execute** — the swappable executors (topological / reactive / stepped / event-driven) + run state (blackboard, control, snapshot, trace, retry).
- **adapter** — the `AiAdapter` abstraction + backends (mock / claude_cli / anthropic_api / openai_compat / claude_terminal).
- **auth** — graph-local credential injection (strategies + `SecretResolver`).
- **handlers** — node implementations registered by name (built-ins).
- **scripting** — sandboxed Rhai (engine + `Value`↔`Dynamic` bridge).
- **registry** — `NodeRegistry`; `with_defaults()` registers the engine's built-in handlers.
- **psflow-run** — the personal-automation runner binary (named-graphs, cross-run-state, run-records, listen-mode, on-failure-hook). The app tier.
- **integrations** — third-party handlers (`composio`, `claude_workflow`) registered **only** by psflow-run, never the engine.
- **wasm** — the `crates/psflow-wasm` member compiling the no-runtime core for the debugger.

## 2. What "good coherence" means here

- **Up-density only.** References point toward the lighter, more-agnostic layer. psflow-run and integrations depend on the engine; the engine never depends on them. (The spine.)
- **The engine is provider-neutral.** No vendor/integration specifics in `graph`…`registry`. New providers/handlers slot in via traits without editing the core.
- **Integrations are isolated by registration**, not directory — they may live under `handlers/` but enter only via psflow-run's `register_integrations`, never `with_defaults()`.
- **The core stays runtime-free.** `graph` + `mermaid` compile without the `runtime` feature (no tokio/reqwest/rhai), so the WASM/embed path holds.

## 3. Characteristic risks

- A **down-density reference**: the engine reaching into `psflow-run` or an integration (the cardinal violation).
- An **integration handler leaking into the engine's default registry** (`with_defaults`) instead of `register_integrations`.
- A **runtime dependency creeping into `graph`/`mermaid`** (a `#[cfg(feature = "runtime")]` that should gate it is missing), breaking the WASM build.
- A handler or adapter **bypassing its trait** (`NodeHandler` / `AiAdapter`) and coupling directly to a concrete backend.
- The hand-maintained **custom serde** on `Graph` drifting from its fields (a new field not threaded through both directions).

## 4. Process flavor

Solo developer; Rust 2021, feature-gated single crate + a `psflow-wasm` member; conventional-commit history; TDD where it pays (round-trip is property-tested with `proptest`, HTTP with `wiremock`). The manifest is hand-maintained against the real module tree and kept honest by `project_coherence_scan` (code is the structural SSOT; this is a human projection of it).

## 5. Authoring the manifest (reference)

Edit `coherence-manifest.json`, then run `project_generate_coherence_map`. Schema: `coherence-manifest.schema.json` (the seeded manifest `$schema`-references it). Unknown/misnamed keys are a hard error the scan surfaces (no silent drop); `sourceNode` is camelCase; `spine.kind` is one of `identity-key` | `direction-rule` | `protocol`; seam `status` is one of `wired` | `partial` | `absent`; `module` may be omitted for an `absent` (unbuilt) seam.
