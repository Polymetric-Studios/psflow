import { findNodeRange, getNodeEvent } from "./state.js";
import type { DebuggerState } from "./state.js";

const container = () => document.getElementById("inspector-content")!;

export function renderInspector(state: DebuggerState): void {
  const el = container();

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

function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
