# ROOT-HANDOVER

As-of: 2026-06-20

## §1 Purpose

The root baton for `ergon/active-documents/` — the cross-session "start here" entry point. Indexes the active threads (one-line status + pointer each); git history is its trail (it never archives). Refresh in place at session-end and bump `As-of:`.

## §3 Status

Active session 2026-06-20: completed the **v1→v2 Ergon master-plan migration** (project_doctor reconform + full content fold). The master-plan tier (charter / plan / decisions DR-001…DR-007 / architecture / primer / terminology / journal) is populated and current; CLAUDE.md + AGENTS.md are managed (v7); code views + CHANGELOG regenerated; legacy `project-data/` archived. **No code changed.** Resume point: triage the five carried-over threads below into `master-plan/plan.md` Now/Next/Later.

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

## §11 History

- 2026-06-20 — Root baton created during the v1→v2 master-plan migration. (No root lead existed under the pre-v2 layout; this is the first.)

## §13 Related

- `master-plan/primer.md` — read-first orientation (links here under §4.3).
- `master-plan/plan.md` — the roadmap these threads triage into.
- `master-plan/journal.md` — the dated history their landings appear in.
