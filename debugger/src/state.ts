import type { ParseResult, TraceResult } from "../pkg/psflow_wasm.js";
import type { NodeRange, TraceEvent } from "../pkg/psflow_wasm.js";

export type NodeState = "idle" | "pending" | "running" | "completed" | "failed" | "cancelled";

export interface DebuggerState {
  /** Loaded .mmd source text */
  source: string | null;
  /** Parse result from WASM */
  parseResult: ParseResult | null;
  /** Loaded trace */
  trace: TraceResult | null;
  /** Current position in trace (event index, -1 = before start) */
  tracePosition: number;
  /** Node states at current trace position */
  nodeStates: Map<string, NodeState>;
  /** Currently selected node ID */
  selectedNodeId: string | null;
  /** Playback state */
  playing: boolean;
  /** Playback speed multiplier */
  speed: number;
  /** Set of breakpointed node IDs */
  breakpoints: Set<string>;
}

export function createState(): DebuggerState {
  return {
    source: null,
    parseResult: null,
    trace: null,
    tracePosition: -1,
    nodeStates: new Map(),
    selectedNodeId: null,
    playing: false,
    speed: 1,
    breakpoints: loadBreakpoints(),
  };
}

function loadBreakpoints(): Set<string> {
  try {
    const raw = localStorage.getItem("psflow-breakpoints");
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) return new Set(parsed);
    }
  } catch { /* ignore corrupted data */ }
  return new Set();
}

export function saveBreakpoints(breakpoints: Set<string>): void {
  localStorage.setItem("psflow-breakpoints", JSON.stringify([...breakpoints]));
}

/** Derive node states from trace events up to the given position. */
export function deriveNodeStates(
  trace: TraceResult,
  position: number
): Map<string, NodeState> {
  const states = new Map<string, NodeState>();
  for (let i = 0; i <= position && i < trace.events.length; i++) {
    const ev = trace.events[i];
    states.set(ev.node_id, ev.state as NodeState);
  }
  return states;
}

/** Get the trace event for a specific node at the current position. */
export function getNodeEvent(
  trace: TraceResult,
  position: number,
  nodeId: string
): TraceEvent | null {
  for (let i = position; i >= 0; i--) {
    if (trace.events[i].node_id === nodeId) {
      return trace.events[i];
    }
  }
  return null;
}

/** Look up NodeRange by id. */
export function findNodeRange(parseResult: ParseResult, nodeId: string): NodeRange | null {
  return parseResult.nodes.find((n) => n.id === nodeId) ?? null;
}
