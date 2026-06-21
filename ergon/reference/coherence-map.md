<!-- argus:generated:start — do not edit; regenerate -->

# Coherence map

Structure generated from `coherence-manifest.json` (DR-029) — do not hand-edit this region; author Fit below.

**Components:** graph, mermaid, execute, adapter, auth, handlers, scripting, registry, psflow-run, integrations, wasm

**Spine:** direction rule — up-density only — psflow-run and integrations depend on the engine, never the reverse; graph/mermaid never pull the runtime

**Source node:** none

| # | Seam | Components | Module | Status | Tested |
|---|------|-----------|--------|--------|--------|
| 1 | mermaid-graph | mermaid ↔ graph | `src/mermaid/loader.rs` | ✓ | ✗ |
| 2 | execute-graph | execute ↔ graph | `src/execute/topological.rs` | ✓ | ✗ |
| 3 | handlers-execute | handlers ↔ execute | `src/handlers/mod.rs` | ✓ | ✗ |
| 4 | handlers-adapter | handlers ↔ adapter | `src/handlers/llm_call.rs` | ✓ | ✗ |
| 5 | handlers-scripting | handlers ↔ scripting | `src/handlers/rhai_handler.rs` | ✓ | ✗ |
| 6 | handlers-auth | handlers ↔ auth | `src/auth/apply_ctx.rs` | ✓ | ✗ |
| 7 | registry-handlers | registry ↔ handlers | `src/registry.rs` | ✓ | ✗ |
| 8 | run-engine | psflow-run ↔ execute | `src/bin/psflow_run.rs` | ✓ | ✗ |
| 9 | run-integrations | integrations ↔ psflow-run | `src/handlers/composio.rs` | ✓ | ✗ |
| 10 | wasm-core | wasm ↔ mermaid | `crates/psflow-wasm` | ✓ | ✗ |

<!-- argus:generated:end -->
## Fit & divergence surface

Last audit: 2026-06-21 — Argus seam-fit audit (`ergon/ephemeral/audits/20260620-224349-argus-audit-coherence-eye/`). The audit found one spine break and three lesser misfits; **all four are now closed** (this session). `project_coherence_scan` is structurally clean (every module path resolves); the post-fix scan flags `handlers-adapter`, `run-integrations`, and `wasm-core` as *changed since review* — expected, they are the seams just edited, cleared by this note on the next commit.

Up-density spine — **holds (re-verified)**. A full-tree sweep confirms no engine-tier file (`graph`/`mermaid`/`execute`/`adapter`/`auth`/`scripting`/`registry`) imports `crate::handlers`. The break the audit caught — `src/auth/registry.rs` importing `crate::handlers::websocket::WS_HANDLER_NAME` to special-case the WS handler by name inside `validate_graph` (a down-density / stratum inversion: the lighter `auth` layer reaching into the denser `handlers` layer) — was closed by relocating the WS-transport auth-compatibility check into `WebSocketHandler::validate_node` (handler → auth, up-density). The auth registry's `validate_graph` now carries only transport-agnostic auth-shape checks (declared-strategy validity + undeclared-reference); the load-time gate is preserved and now actually runs in the live per-handler validation pass.

Seam 10 (`wasm-core`) retargeted `wasm ↔ graph` → **`wasm ↔ mermaid`**: the wasm crate imports only `psflow::mermaid::parse`, never `graph`. The load-bearing invariant (the no-runtime core compiles for WASM) holds via `mermaid`. The wasm node-range sort gained a `(definition.from, id)` tie-break, making co-line node order total and deterministic (the previously-flaky `definition_spans_have_ascending_from_offsets` now passes across repeated process runs).

Stale doc-descriptions fixed: `src/handlers/llm_call.rs` cache-boundary comment (`{{cache_boundary}}` → the real sentinel `<<<cache_boundary>>>`) and `src/handlers/composio.rs` docblock (now states the true contract — registered only by psflow-run via `register_integrations`, never the engine's `with_defaults`, hence unavailable on the stock `psflow` binary).

Standing watch-item: `run-integrations` — `composio`/`claude_workflow` must keep entering only via psflow-run's `register_integrations`, never `registry::with_defaults`. (Test paths are unmapped — the ✗ Tested column means no test declared in the manifest, not that coverage is absent.)

## Divergence surface

_None — all four audit misfits closed 2026-06-21._
