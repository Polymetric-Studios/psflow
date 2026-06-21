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
| 10 | wasm-core | wasm ↔ graph | `crates/psflow-wasm` | ✓ | ✗ |

<!-- argus:generated:end -->
## Fit & divergence surface

Initial baseline, 2026-06-20 (seeded from the v2 architecture pass). All 10 seams are **wired** and every module path resolves (`project_coherence_scan`: 0 findings). The up-density spine holds — the engine (`graph`…`registry`) carries no reference to `psflow-run` or `integrations`, and `graph`/`mermaid` stay behind the `runtime`-feature line. The seam to watch is `run-integrations`: `composio`/`claude_workflow` must keep entering only via psflow-run's `register_integrations`, never `registry::with_defaults`. (Test paths are unmapped — the ✗ Tested column means no test declared in the manifest, not that coverage is absent.)

## Divergence surface

_None._
