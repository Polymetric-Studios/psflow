# Journal

Last-Reviewed: 2026-06-21

The **single source of truth for this project's history** — the tagged, chronological journal of every notable change, at the right altitude (what the project can do / does differently / no longer does; plan transitions; knowledge changes), not implementation detail (commit history is that). `CHANGELOG.md` at the repo root is **generated** from this file by the `generate_changelog` action (the DR-011 generated-view discipline applied to history) — edit this file, never `CHANGELOG.md`.

## Entry format

One entry per line under `## Entries`, reverse-chronological (newest first):

```
- YYYY-MM-DD · Kind · [tags] text (DR-NNN)
```

- **Kind** (the lane + section): product — `Added` (new capability) · `Changed` (does it differently) · `Deprecated` (will be removed) · `Removed` (no longer does) · `Fixed` · `Security`; plan — `Plan` (milestone met/added/modified, roadmap reshaped); knowledge — `Docs` (a recorded `DR`, a design doc/spec landed or materially reshaped, an SSOT realigned).
- **tags** (optional, inline): `[capability]` when it changes what the project can or can't do; `[breaking]` for breaking changes; `[internal]` for a product change with no end-user-facing effect (journaled, but withheld from `CHANGELOG.md`). Reading just the `[capability]` lines gives the big-picture "what this project does and doesn't."
- **pointer** (optional): `(DR-NNN)` or a durable doc path for the why/detail — prefer `decisions.md` (stable) over `active-documents/` docs that archive.

**CHANGELOG.md is the user-facing subset.** `generate_changelog` renders only the **product** Kinds (`Added`/`Changed`/`Deprecated`/`Removed`/`Fixed`/`Security`); the `Plan` and `Docs` lanes and any `[internal]`-tagged entry stay journal-only. The journal is the full history; the changelog is what touches the user side.

**Releases (optional).** A release-divider line inside `## Entries` marks a version boundary:

```
### [VERSION] - YYYY-MM-DD
```

Everything above the first divider is `[Unreleased]`; entries between divider N and N+1 belong to release N. `VERSION` is an opaque label (passed through verbatim — never parsed or sorted; journal order is trusted). `generate_changelog` renders one Keep-a-Changelog section per bucket. Cut a release with `journal_cut_release` (inserts the divider at the top of `## Entries`); a journal with no dividers renders as a single `[Unreleased]` section.

**Forgiving, not brittle.** This file is hand-maintained and human-first. A line that doesn't match the format is simply ignored by the generator — it never corrupts the journal. Capture beats precision: record the event even if a tag is imperfect. There is no gate; the habit is the `ergon-session-end` capture, and an `update-master-plan` review can backfill from `git log` if a session is skipped.

## Entries

- 2026-06-21 · Fixed · Debugger wasm node ordering is now deterministic — co-line node definitions get a `(definition.from, id)` sort tie-break (was HashMap-iteration-order flaky).
- 2026-06-21 · Fixed · [internal] Closed an up-density spine break the Argus seam-fit audit surfaced: `auth` no longer imports a `handlers` const — the WS-transport auth-compatibility cross-check moved into `WebSocketHandler::validate_node` (handler→auth), where it now actually runs in the live load-time validation pass.
- 2026-06-21 · Docs · [internal] Argus seam-fit audit: retargeted the `wasm-core` seam to `wasm ↔ mermaid` (the real import surface), refreshed the coherence-map Fit & divergence surface, and corrected two stale module doc-descriptions (`llm_call` cache-boundary sentinel; `composio` registration contract).
- 2026-06-20 · Docs · [internal] Adopted the Argus coherence eye: authored the seam manifest (11 components, the up-density spine, 10 wired seams) + domain brief in `reference/`; `project_coherence_scan` clean.
- 2026-06-20 · Docs · [internal] Migrated to the Ergon v2 master-plan layout: seeded charter/plan/decisions (DR-001…DR-007)/architecture/primer/terminology/journal from legacy `project-data/` + the live source, reseeded CLAUDE.md/AGENTS.md as managed, extracted the Debugging note to `reference/`, and created the root baton.
- 2026-06-19 · Added · [capability] OpenRouter (and any OpenAI-wire provider — OpenAI/Groq/Together/local) via the generic `openai_compat` adapter; arbitrary-model access through per-node `config.model`. (DR-005)
- 2026-06-09 · Added · `anthropic_api` adapter emits `output_config.format` for structured (JSON-schema) outputs.
- 2026-06-07 · Added · `claude_terminal` resume mode + child-env seam on `SessionOptions`.
- 2026-06-07 · Added · `llm_call` `config.output_key` names the result port.
- 2026-06-06 · Added · `llm_call` `template_render` config for verbatim prompts.
- 2026-06-02 · Added · [capability] `claude_workflow` handler drives the real `claude` TUI headless over a PTY, with approval-dialog routing (in-session file-channel `approval=ask`) and configurable remote control. (terminal feature)
- 2026-06-02 · Added · [capability] `loop` handler — accumulate with `until` / `until_dry` / `max`; the loop family (map + subgraph_invoke + loop) wired into psflow-run. (DR-006)
- 2026-06-02 · Added · [capability] `map` handler — data-driven fan-out over a runtime list, concurrency-capped and order-preserved.
- 2026-06-01 · Changed · [capability] Isolated Composio as a removable integration and made the core provider-neutral — the engine vs psflow-run app split. (DR-004)
- 2026-06-01 · Added · [capability] psflow-run personal runner: named-graphs with runtime-inputs, LLM adapter wiring, run-records, notify-on-failure, cross-run-state (scheduled idempotency), config SSOT + graph discovery + fail-fast validation.
- 2026-06-01 · Added · [capability] psflow-run trigger bridge / listen-mode (Composio `dev listen` → graph dispatch), incl. an org-free receive path for personal accounts.
- 2026-06-01 · Added · Native Composio handler (CLI-wrapping, structured outputs) with record/replay + TTL tool-response-cache; API key read from the macOS keychain.
- 2026-06-01 · Added · `schema()` for all default handlers + a drift guard.
- 2026-06-01 · Docs · Initialized Ergon and populated the project terminology.
- 2026-05-30 · Added · Reactive WS handshake on the `ws` handler and `cookie_jar` CSRF cookie→header echo (auth-strategy gaps closed).
- 2026-05-30 · Changed · [internal] Moved web-API spec docs and the Magnific wrapper out of psflow into Ergon — keeping the core provider-neutral.
- 2026-03-28 · Added · [capability] Engine foundation (through "Phase 3"): the petgraph-backed `graph` data model with portable serde (DR-002), the annotated-Mermaid single-file format with round-trip load/export (DR-001), the feature-gated core/runtime split (DR-003), swappable executors (DR-006), and the `AiAdapter` abstraction (DR-005).
