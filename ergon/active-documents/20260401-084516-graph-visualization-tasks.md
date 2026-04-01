# 20260401-084516-graph-visualization-tasks.md

# Graph Visualization View — Task List

**Split from:** `20260328-082824-process-control-framework-tasks.md` (section 5.7)
**Updated:** 2026-04-01

---

## Overview

An interactive SVG graph view alongside the existing CodeMirror text editor. ELK.js computes hierarchical layout (with first-class compound node support for subgraphs); a thin custom SVG renderer (~300-500 LOC) draws the positioned graph. Nodes are styled via real CSS classes matching the existing execution state decorations. Click a node in either view to select it in both.

**Stack:** ELK.js (layout engine, ~150KB), custom SVG renderer (vanilla TS), CSS for state styling.

**Architecture:** `psflow_wasm.parse_mmd()` → graph JSON → ELK.js layout (x/y positions) → custom SVG render → DOM with zoom/pan/click/CSS state transitions. CodeMirror remains the text panel; the graph view is a second panel.

---

## Tasks

| ID | | Task | Details / Acceptance Criteria | Pri |
|----|---|------|-------------------------------|-----|
| 5.7.1 | [x] | ELK.js integration | Add `elkjs` dependency. Write a `layoutGraph()` function that converts `ParseResult` (nodes, edges, subgraphs) into an ELK graph JSON object with compound nodes for subgraphs, calls `elk.layout()`, and returns positioned elements with x/y/width/height. Handle edge routing waypoints | P1 |
| 5.7.2 | [x] | SVG graph renderer | Custom SVG renderer: `<rect>` + `<text>` for nodes, `<path>` for edges (using ELK waypoints), `<g>` with background `<rect>` for subgraphs. Render into a container `<div>` alongside CodeMirror. Re-render on graph change. Clean separation: layout module produces positions, renderer consumes them | P1 |
| 5.7.3 | [x] | Zoom and pan | SVG viewBox-based zoom/pan. Mouse wheel to zoom, click-drag on background to pan. Fit-to-view button. Minimap optional (P2). Keyboard: `0` to reset zoom, `+`/`-` to zoom in/out | P1 |
| 5.7.4 | [x] | Node interaction | Click node in SVG to select it — highlights in both SVG and CodeMirror editor. Hover shows tooltip (same data as CodeMirror hover tooltip 5.4.3). Double-click scrolls CodeMirror to that node's source. Selected node state shared with existing `DebuggerState` | P1 |
| 5.7.5 | [x] | Execution state styling | Apply CSS classes to SVG node elements matching the existing state classes (`.node-running`, `.node-completed`, `.node-failed`, etc.). Reuse the same color palette. Animate running state (subtle pulse). Update on trace position change via the same `StateEffect` pipeline | P1 |
| 5.7.6 | [x] | Panel layout integration | Split-pane layout: CodeMirror (left) + SVG graph (right), with draggable divider. Toggle between text-only, graph-only, and split views. Persist layout preference in localStorage | P1 |
| 5.7.7 | [x] | Edge labels and port display | Show edge labels on paths. Optionally show port names on node borders (input ports left/top, output ports right/bottom). Toggle port visibility via toolbar | P2 |

---

## Dependencies

- Mermaid parser (1.2) — provides `parse_mmd()` via WASM
- WASM parser build (5.1.1) — the `ParseResult` type with node/edge/subgraph data
- Execution state decorations (5.2.2) — CSS classes reused for SVG styling
- Trace playback (5.3) — `StateEffect` pipeline drives SVG state updates
- Inspector / selection model (5.4) — shared `DebuggerState` for cross-view selection

## Priority Key

| Priority | Meaning |
|----------|---------|
| **P1** | Important for production use. Needed before the framework is truly general-purpose. |
| **P2** | Future / nice-to-have. Visual tooling, Python bindings, cross-compilation. |
