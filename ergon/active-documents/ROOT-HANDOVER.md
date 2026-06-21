# ROOT-HANDOVER

As-of: 2026-06-21

## §1 Purpose

The root baton for `ergon/active-documents/` — the cross-session "start here" entry point. Indexes the active threads (one-line status + pointer each); git history is its trail (it never archives). Refresh in place at session-end and bump `As-of:`.

## §3 Status

Session 2026-06-21 (closed): ran the **Argus seam-fit audit** (10 parallel `argus` seam judgments + whole-eye synthesis) and applied every fix it surfaced (`554e34e8`). Headline: closed a real **up-density spine break** — `src/auth/registry.rs` imported `crate::handlers::websocket::WS_HANDLER_NAME`; the WS-transport auth-compatibility check now lives in `WebSocketHandler::validate_node` (handler→auth, up-density), where it also actually runs in the live load-time pass. Plus: a deterministic wasm node-sort `(definition.from, id)` tie-break, the `wasm-core` seam retargeted `wasm ↔ mermaid` (real import surface), and two stale module doc-descriptions corrected. Tests: 854 lib + all integration suites green; wasm 15/15 deterministic across processes. `project_coherence_scan` clean; coherence-map Fit surface refreshed; journal + CHANGELOG updated. Audit artifacts: `ergon/ephemeral/audits/20260620-224349-argus-audit-coherence-eye/` (gitignored). **Resume point:** (carried over) triage the five threads below into `master-plan/plan.md` Now/Next/Later; `main` is ahead of `origin/main` — push pending.

## §4 Active threads

### §4.1 Thread index

The five docs below predate the v2 migration and were carried over as flat files in `active-documents/`. Their live status was **not** verified during the migration — open each to resume. Triaging them into `master-plan/plan.md` is a backlog item.

- **Composio ↔ psflow integration design** — `20260601-091344-composio-psflow-integration-design.md`. Related landings (journal 2026-06-01): native composio handler, integration isolation, trigger bridge / listen-mode. Likely substantially realized; confirm remaining scope.
- **Composition handlers follow-ups** — `20260602-100350-Composition-Handlers-Followups.md`. The map / loop / subgraph_invoke family wired into psflow-run on 2026-06-02; this tracks the follow-ups.
- **psflow network-slice follow-ups** — `20260424-091430-psflow-network-slice-followups.md`. Related: the WS-handshake / cookie_jar auth work (journal 2026-05-30).
- **Graph visualization tasks** — `20260401-084516-graph-visualization-tasks.md`. The WASM `debugger/` now exists; confirm what remains.
- **Process-control-framework tasks** — `20260328-082824-process-control-framework-tasks.md`. The oldest; the `execute/` layer it concerns is built — likely mostly done.

## §6 Open questions

- The five carried-over threads' live status is unverified — which are done (→ archive), active (→ thread + plan), or abandoned (→ plan "Won't")? Migration-seed; owner to confirm.
- Should the multi-doc efforts above become proper `{ts}-{Subject}/` threads (each with its own `{ts}-Master.md` lead) per the v2 thread convention? They are currently flat files.

## §8 Drift / hand-off

- Scans clean at close: `project_master_plan_scan` 0 findings (code-manifest / architecture-map regenerated this session); `project_coherence_scan` 0 findings (adopted).
- **Ergon workflow-runner bug — reported to the Ergon project, tracked there (not a psflow-repo bug).** Supervised `workflow_run` of `argus-audit` auto-completed `0/5` with no agent execution and a false `completed` status; the deterministic `dry_run` rendered the full plan fine. Likely cause (unconfirmed against source): the step-classifier mis-reads the two `argus` agent steps — `seam-fit-judge` is nested inside a `parallel-loop` subgraph — as deterministic, so it runs the deterministic prefix and marks complete without handing off. Workaround used: drove the same per-seam fan-out + whole-eye synthesis via the Claude Code Workflow tool with `agentType: 'argus'`.
- `main` is ahead of `origin/main` (this session's `554e34e8` + the session-end capture) — push state per session-end.
- Code-hygiene (carried over, not this session's scope): 11 Rust files lack a `//!` module docblock (DR-013) — `code-manifest.md` flags them; `lint_docblocks` lists them.
- Argus seam **test paths are unmapped** (Tested ✗ = undeclared, not uncovered) — add to `reference/coherence-manifest.json` if wanted.

## §11 History

- 2026-06-20 — Root baton created during the v1→v2 master-plan migration (the first root lead under v2).
- 2026-06-20 — Landed: v2 master-plan migration (`0d66ae64`) + Argus coherence eye (`c238e7dd`).
- 2026-06-21 — Landed: Argus seam-fit audit + all five fixes (`554e34e8`) — spine break closed, wasm sort deterministic, `wasm-core` seam retargeted `wasm ↔ mermaid`, two doc drifts fixed; journal + CHANGELOG + coherence-map refreshed. Session-end: regenerated code-manifest / architecture-map; surfaced the Ergon workflow-runner bug (§8, reported to Ergon).

## §13 Related

- `master-plan/primer.md` — read-first orientation (links here under §4.3).
- `master-plan/plan.md` — the roadmap these threads triage into.
- `master-plan/journal.md` — the dated history their landings appear in.
