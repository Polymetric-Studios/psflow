import { findNodeRange, getNodeEvent } from "./state.js";
import type { DebuggerState } from "./state.js";
const container = () => document.getElementById("inspector-content")!;

let searchFilter = "";

type InspectorTab = "node" | "blackboard" | "console" | "breakpoints";
let activeTab: InspectorTab = "node";
let onUpdateCallback: (() => void) | null = null;

export function setInspectorOnUpdate(cb: () => void): void {
  onUpdateCallback = cb;
}

let lastRenderedTab: InspectorTab | null = null;
let lastBpCount = -1;

export function renderInspector(state: DebuggerState): void {
  const el = container();
  const header = document.getElementById("inspector-header")!;

  // Only rebuild tab bar when tab or breakpoint count changes
  const bpCount = state.breakpoints.size;
  if (activeTab !== lastRenderedTab || bpCount !== lastBpCount) {
    lastRenderedTab = activeTab;
    lastBpCount = bpCount;

    const bpBadge = bpCount > 0 ? `<span class="tab-badge">${bpCount}</span>` : "";
    header.innerHTML = `
      <div class="inspector-tabs">
        <button class="tab${activeTab === "node" ? " active" : ""}" data-tab="node">Node</button>
        <button class="tab${activeTab === "blackboard" ? " active" : ""}" data-tab="blackboard">Blackboard</button>
        <button class="tab${activeTab === "console" ? " active" : ""}" data-tab="console">Console</button>
        <button class="tab${activeTab === "breakpoints" ? " active" : ""}" data-tab="breakpoints">Breakpoints${bpBadge}</button>
      </div>
    `;

    for (const btn of header.querySelectorAll<HTMLButtonElement>(".tab")) {
      btn.addEventListener("click", () => {
        activeTab = btn.dataset.tab as InspectorTab;
        renderInspector(state);
      });
    }
  }

  if (activeTab === "blackboard") {
    renderBlackboard(el, state);
  } else if (activeTab === "console") {
    renderConsole(el, state);
  } else if (activeTab === "breakpoints") {
    renderBreakpointList(el, state);
  } else {
    renderNodeInspector(el, state);
  }
}

function renderNodeInspector(el: HTMLElement, state: DebuggerState): void {
  if (!state.selectedNodeId || !state.parseResult) {
    el.innerHTML = `<p class="placeholder">Select a node to inspect</p>`;
    return;
  }

  const nodeRange = findNodeRange(state.parseResult, state.selectedNodeId);
  if (!nodeRange) {
    el.innerHTML = `<p class="placeholder">Node not found</p>`;
    return;
  }

  const nodeState = state.nodeStates.get(state.selectedNodeId) ?? "idle";
  const traceEvent = state.trace
    ? getNodeEvent(state.trace, state.tracePosition, state.selectedNodeId)
    : null;

  let html = "";

  // Node identity
  html += `<div class="inspector-section">
    <h3>Node</h3>
    <div class="inspector-row"><span class="key">ID</span><span class="val">${esc(nodeRange.id)}</span></div>
    <div class="inspector-row"><span class="key">Label</span><span class="val">${esc(nodeRange.label)}</span></div>
    <div class="inspector-row"><span class="key">State</span><span class="val">${esc(nodeState)}</span></div>
  </div>`;

  // Annotations (static config)
  if (nodeRange.annotations.length > 0) {
    html += `<div class="inspector-section"><h3>Config</h3>`;
    for (const ann of nodeRange.annotations) {
      html += `<div class="inspector-row"><span class="key">${esc(ann.key)}</span><span class="val">${esc(ann.value)}</span></div>`;
    }
    html += `</div>`;
  }

  // Runtime trace data
  if (traceEvent) {
    html += `<div class="inspector-section"><h3>Execution</h3>`;
    html += `<div class="inspector-row"><span class="key">Order</span><span class="val">#${traceEvent.order}</span></div>`;
    if (traceEvent.elapsed_ms !== undefined) {
      html += `<div class="inspector-row"><span class="key">Duration</span><span class="val">${traceEvent.elapsed_ms.toFixed(1)} ms</span></div>`;
    }
    if (traceEvent.error) {
      html += `<div class="inspector-row"><span class="key">Error</span><span class="val" style="color: var(--state-failed-border)">${esc(traceEvent.error)}</span></div>`;
    }
    html += `</div>`;

    // Outputs
    if (traceEvent.outputs_json) {
      html += `<div class="inspector-section"><h3>Outputs</h3>`;
      try {
        const formatted = JSON.stringify(JSON.parse(traceEvent.outputs_json), null, 2);
        html += `<pre class="inspector-json">${esc(formatted)}</pre>`;
      } catch {
        html += `<pre class="inspector-json">${esc(traceEvent.outputs_json)}</pre>`;
      }
      html += `</div>`;
    }
  }

  el.innerHTML = html;
}

// --- Blackboard inspector ---

interface BlackboardEntry {
  key: string;
  value: unknown;
  scope: string;
  nodeId: string;
  changed: boolean;
}

/** Accumulate outputs from all events up to the current position to reconstruct blackboard state. */
function deriveBlackboardEntries(state: DebuggerState): BlackboardEntry[] {
  if (!state.trace) return [];

  // Track accumulated state: scope → key → { value, nodeId }
  const accumulated = new Map<string, Map<string, { value: unknown; nodeId: string }>>();

  // Determine which keys changed at the current step
  const changedKeys = new Set<string>();

  for (let i = 0; i <= state.tracePosition && i < state.trace.events.length; i++) {
    const ev = state.trace.events[i];
    if (ev.state !== "completed" || !ev.outputs_json) continue;

    let outputs: Record<string, unknown>;
    try {
      outputs = JSON.parse(ev.outputs_json);
    } catch {
      continue;
    }

    const scope = `node:${ev.node_id}`;
    if (!accumulated.has(scope)) accumulated.set(scope, new Map());
    const scopeMap = accumulated.get(scope)!;

    for (const [key, value] of Object.entries(outputs)) {
      scopeMap.set(key, { value, nodeId: ev.node_id });

      // Also accumulate into "global" view
      if (!accumulated.has("global")) accumulated.set("global", new Map());
      accumulated.get("global")!.set(`${ev.node_id}.${key}`, { value, nodeId: ev.node_id });

      if (i === state.tracePosition) {
        changedKeys.add(`${scope}:${key}`);
        changedKeys.add(`global:${ev.node_id}.${key}`);
      }
    }
  }

  // Flatten to entries
  const entries: BlackboardEntry[] = [];
  for (const [scope, scopeMap] of accumulated) {
    for (const [key, { value, nodeId }] of scopeMap) {
      entries.push({
        key,
        value,
        scope,
        nodeId,
        changed: changedKeys.has(`${scope}:${key}`),
      });
    }
  }

  return entries;
}

function renderBlackboard(el: HTMLElement, state: DebuggerState): void {
  saveSearchState();
  if (!state.trace || state.tracePosition < 0) {
    el.innerHTML = `<p class="placeholder">Load a trace to inspect blackboard state</p>`;
    return;
  }

  const entries = deriveBlackboardEntries(state);

  let html = `<div class="inspector-section">
    <input type="text" id="bb-search" class="bb-search" placeholder="Search keys..." value="${esc(searchFilter)}" />
  </div>`;

  if (entries.length === 0) {
    html += `<p class="placeholder">No data at this position</p>`;
    el.innerHTML = html;
    wireSearch(el, state);
    return;
  }

  // Group by scope
  const scopes = new Map<string, BlackboardEntry[]>();
  for (const entry of entries) {
    if (searchFilter && !entry.key.toLowerCase().includes(searchFilter.toLowerCase())) continue;
    if (!scopes.has(entry.scope)) scopes.set(entry.scope, []);
    scopes.get(entry.scope)!.push(entry);
  }

  // Render global scope first, then node scopes
  const scopeOrder = [...scopes.keys()].sort((a, b) => {
    if (a === "global") return -1;
    if (b === "global") return 1;
    return a.localeCompare(b);
  });

  for (const scope of scopeOrder) {
    const scopeEntries = scopes.get(scope)!;
    const label = scope === "global" ? "Global" : scope.replace("node:", "");
    html += `<div class="inspector-section"><h3>${esc(label)}</h3>`;
    for (const entry of scopeEntries) {
      const valStr = formatValue(entry.value);
      const changedClass = entry.changed ? " bb-changed" : "";
      html += `<div class="inspector-row${changedClass}">
        <span class="key">${esc(entry.key)}</span>
        <span class="val">${esc(valStr)}</span>
      </div>`;
    }
    html += `</div>`;
  }

  if (scopeOrder.length === 0) {
    html += `<p class="placeholder">No matches</p>`;
  }

  el.innerHTML = html;
  wireSearch(el, state);
}

function wireSearch(el: HTMLElement, state: DebuggerState): void {
  const input = el.querySelector<HTMLInputElement>("#bb-search");
  if (input) {
    input.addEventListener("input", () => {
      searchFilter = input.value;
      renderBlackboard(el, state);
    });
    // Restore focus and cursor position after re-render
    if (document.activeElement?.id === "bb-search" || lastSearchWasFocused) {
      const pos = lastSearchCursor;
      input.focus();
      input.setSelectionRange(pos, pos);
    }
  }
}

let lastSearchWasFocused = false;
let lastSearchCursor = 0;

function saveSearchState(): void {
  const input = document.getElementById("bb-search") as HTMLInputElement | null;
  lastSearchWasFocused = document.activeElement === input;
  lastSearchCursor = input?.selectionStart ?? 0;
}

function formatValue(v: unknown): string {
  if (v === null) return "null";
  if (v === undefined) return "undefined";
  if (typeof v === "string") return v.length > 80 ? v.slice(0, 77) + "..." : v;
  if (typeof v === "object") {
    const s = JSON.stringify(v);
    return s.length > 80 ? s.slice(0, 77) + "..." : s;
  }
  return String(v);
}

// --- Console output ---

function renderConsole(el: HTMLElement, state: DebuggerState): void {
  if (!state.trace || state.tracePosition < 0) {
    el.innerHTML = `<p class="placeholder">Run or load a trace to see output</p>`;
    return;
  }

  let html = `<div class="console-output">`;

  for (let i = 0; i <= state.tracePosition && i < state.trace.events.length; i++) {
    const ev = state.trace.events[i];
    const stateClass = ev.state;
    const isCurrent = i === state.tracePosition;

    html += `<div class="console-entry${isCurrent ? " console-current" : ""}">`;
    html += `<div class="console-header">`;
    html += `<span class="console-dot ${esc(stateClass)}"></span>`;
    html += `<span class="console-node">${esc(ev.node_id)}</span>`;
    html += `<span class="console-state">${esc(ev.state)}</span>`;
    if (ev.elapsed_ms !== undefined) {
      html += `<span class="console-time">${ev.elapsed_ms.toFixed(1)}ms</span>`;
    }
    html += `</div>`;

    if (ev.error) {
      html += `<div class="console-error">${esc(ev.error)}</div>`;
    }

    if (ev.outputs_json && ev.state === "completed") {
      try {
        const outputs = JSON.parse(ev.outputs_json) as Record<string, unknown>;
        for (const [key, val] of Object.entries(outputs)) {
          const valObj = val as { type?: string; value?: unknown };
          if (valObj && valObj.value !== undefined && valObj.value !== null) {
            html += `<div class="console-output-row">`;
            html += `<span class="console-key">${esc(key)}</span>`;
            html += `<span class="console-val">${esc(formatValue(valObj.value))}</span>`;
            html += `</div>`;
          }
        }
      } catch { /* skip malformed */ }
    }

    html += `</div>`;
  }

  html += `</div>`;
  el.innerHTML = html;

  // Auto-scroll to bottom
  el.scrollTop = el.scrollHeight;
}

// --- Breakpoint list ---

function renderBreakpointList(el: HTMLElement, state: DebuggerState): void {
  if (state.breakpoints.size === 0) {
    el.innerHTML = `<p class="placeholder">No breakpoints set. Click a gutter marker to add one.</p>`;
    return;
  }

  let html = `<div class="inspector-section"><h3>Active Breakpoints</h3>`;
  for (const nodeId of [...state.breakpoints].sort()) {
    const nodeState = state.nodeStates.get(nodeId) ?? "idle";
    html += `<div class="inspector-row bp-row" data-node="${esc(nodeId)}">
      <span class="val"><span class="cm-breakpoint-dot inline"></span> ${esc(nodeId)}</span>
      <span class="key">${esc(nodeState)}</span>
    </div>`;
  }
  html += `</div>`;

  el.innerHTML = html;

  // Click to select node and switch to Node tab
  for (const row of el.querySelectorAll<HTMLElement>(".bp-row")) {
    row.style.cursor = "pointer";
    row.addEventListener("click", () => {
      const nodeId = row.dataset.node;
      if (nodeId) {
        state.selectedNodeId = nodeId;
        activeTab = "node";
        onUpdateCallback?.();
      }
    });
  }
}

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
