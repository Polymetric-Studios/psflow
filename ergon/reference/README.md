# Reference

Last-Reviewed: 2026-06-11

Durable, **descriptive** project knowledge the LLM should read when working in
the relevant area — cross-cutting explainers and external API / protocol specs
that have no single owning component. A standing `ergon/` tier alongside
`master-plan/` (the prescriptive plan of record) and `active-documents/`
(ephemeral session work).

## What belongs here

- Cross-cutting explainers that span components (no single owning component).
- External API / protocol / vendor-spec references the project depends on.

## What does NOT belong here

- **Single-component domain knowledge** → that component's `README` / docblock.
- **Decisions / direction / terms / system shape** → `master-plan/`.
- **In-flight session work** → `active-documents/` (it archives; this does not).

## Convention

- **Topic-named, no timestamps** (`spotify-dealer-protocol.md`) — standing
  knowledge, not session artifacts.
- **Carries `Last-Reviewed: YYYY-MM-DD`** (line 3) — `master_plan_scan` covers
  this folder.
- **Exempt from the canonical 13-section spine** — a reference doc is content +
  `Last-Reviewed:`, not a plan/ADR.
- Linked from `master-plan/primer.md` so it is part of read-first orientation.

This README is the tier's placeholder — it keeps the folder present even when no
reference docs exist yet. Add topic docs alongside it.
