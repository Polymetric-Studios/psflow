# Plan

Last-Reviewed: 2026-06-20

## §1 Preamble

The sequenced plan of record. The charter states intent; this states order. Each item references an `M-NNN` from the Milestones table. **Seeded 2026-06-20** from observed state (git history + the active-documents threads) during the v1→v2 master-plan migration — the Now/Next/Later sequencing is inferred and should be confirmed by the owner; the active threads themselves are indexed in `active-documents/ROOT-HANDOVER.md`.

## §9 Plan

### §9.1 Planned tasks

**Now**
- [ ] psflow-run automation surface — Composio integration + composition handlers (M-004). See `active-documents/20260601-091344-composio-psflow-integration-design.md` and `20260602-100350-Composition-Handlers-Followups.md`.
- [ ] WASM debugger / graph visualization (M-005). See `active-documents/20260401-084516-graph-visualization-tasks.md`.

**Next**
- [ ] Network-slice follow-ups (M-004). See `active-documents/20260424-091430-psflow-network-slice-followups.md`.
- [ ] Process-control-framework tasks (M-002 / M-004). See `active-documents/20260328-082824-process-control-framework-tasks.md`.

**Later**
- [ ] Host bindings — C-FFI/Unity and PyO3 (M-006). Targets named in the project description; no crate exists yet (only `crates/psflow-wasm`).

## §11 History

### §11.2 Deferred / superseded

**Won't**
- _None recorded yet._

## Milestones

| id | title | status | exit-criteria | shipped-on |
|----|-------|--------|---------------|------------|
| M-001 | Core graph model + annotated-Mermaid round-trip | shipped | `load_mermaid`/`export_mermaid` round-trip structurally equivalent; validation collects all errors | 2026-03-28 |
| M-002 | Execution engine + swappable executors | shipped | topological/reactive/stepped/event_driven over one `Graph`; blackboard, control, snapshot, trace | 2026-03-28 |
| M-003 | AI adapter abstraction + backends | shipped | `AiAdapter` trait + mock/claude_cli/anthropic_api/openai_compat/claude_terminal; ancestor-scoped conversation-history | — |
| M-004 | psflow-run automation (auth, integrations, scheduling) | active | named-graphs, cross-run-state, run-records, listen-mode, on-failure-hook; composio + claude_workflow integrations; auth-strategies | — |
| M-005 | WASM debugger | active | `psflow-wasm` builds core to WASM; `debugger/` observes runs via `debug_server`/`event_bus` | — |
| M-006 | Host bindings (C-FFI/Unity, PyO3) | planned | the core embeddable from a native host and from Python | — |

## Backlog

Things to do later — captured the moment they come up so they aren't lost, and triaged into the Now/Next/Later blocks and the Milestones table when picked up. Unsequenced; each is a `- [ ]` item with enough context to act on later.

- [ ] Confirm/triage the five carried-over active-documents threads (see `ROOT-HANDOVER.md`) into the Now/Next/Later blocks above — their live status was not verified during the migration seed.
- [ ] Regenerate the code views (`psflow-manifest` / `generate_code_manifest`) and keep `architecture.md` §13 pointing at fresh output.
