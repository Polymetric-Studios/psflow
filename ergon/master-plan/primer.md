# Primer

Last-Reviewed: 2026-06-20

## 1. Preamble

### 1.2 Purpose

The read-first orientation index. An isolated session or sub-agent reads this first to orient cheaply — it **links** to the canonical sources rather than restating them, so it stays thin and cannot drift. Enforced by the read-before-work hook.

## 4. Body

### 4.1 Prime directive

psflow is a **domain-agnostic graph execution engine**: one annotated-Mermaid (`.mmd`) file holds a graph's topology, typing, node implementation, and execution semantics, and a runtime-selected executor (topological / reactive / stepped / event-driven) walks it. The engine (the `psflow` library) is provider-neutral and feature-gated so the `graph` + `mermaid` core compiles without the async stack; **psflow-run** is the personal-automation runner built on top (Composio triggers, scheduled named-graphs, PTY-driven `claude`). **Prime directive:** keep the engine domain-agnostic and the dependency direction *up-density only* — integrations and the runner depend on the engine, never the reverse; a new provider, handler, or integration must slot in without editing the core.

### 4.2 Orientation index

- **System shape (the why):** `master-plan/architecture.md` — layers, the feature-gated core/runtime seam, allowed dependency directions, runtime model.
- **What it can do + how to use it:** `ergon action="help"` — the live catalog; the `justfile` — build/run/`graph`/`debugger` commands; each skill's body carries its how-to.
- **Intent + boundaries:** `master-plan/charter.md`.
- **Load-bearing decisions:** `master-plan/decisions.md` (DR-001…DR-007).
- **How we work:** `CLAUDE.md` Ethos + Creed (cross-project, not a per-project doc).
- **Terminology:** `master-plan/terminology.md` — read before any work.
- **Reference knowledge:** `reference/` — cross-cutting explainers + external API/protocol specs (when present); single-module knowledge lives in that module's docblock instead.

### 4.3 Read-next (ordered)

1. `master-plan/terminology.md` — the shared vocabulary.
2. The §4.2 links relevant to the task at hand.
3. The active thread baton — start from `active-documents/ROOT-HANDOVER.md`.

## 7. Usage

Read first at session and sub-agent start. The read-before-work hook injects this file's path + a short digest; follow the links for the depth the task needs.
