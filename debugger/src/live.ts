/**
 * Live WebSocket connection to a running psflow debug server.
 *
 * Connects to the engine's --debug-ws server and provides
 * step/resume/pause/cancel commands with real-time event streaming.
 */

import type { NodeState } from "./state.js";

// --- Protocol types (mirror Rust debug_server.rs) ---

interface ServerGraph {
  type: "graph";
  source: string;
}

interface DebugEvent {
  node_id: string;
  from_state: string;
  to_state: string;
  elapsed_ms?: number;
  outputs_json?: string;
  error?: string;
}

interface ServerEvents {
  type: "events";
  events: DebugEvent[];
}

interface ServerPaused {
  type: "paused";
}

interface ServerResumed {
  type: "resumed";
}

interface ServerComplete {
  type: "complete";
  trace_json: string;
}

interface ServerError {
  type: "error";
  message: string;
}

type ServerMsg = ServerGraph | ServerEvents | ServerPaused | ServerResumed | ServerComplete | ServerError;

export type LiveStatus = "disconnected" | "connecting" | "paused" | "running" | "complete";

export interface LiveCallbacks {
  /** Called when the graph source is received from the server. */
  onGraph(source: string): void;
  /** Called when execution events arrive. */
  onEvents(events: DebugEvent[]): void;
  /** Called when execution state changes (paused, running, complete, disconnected). */
  onStatusChange(status: LiveStatus): void;
  /** Called when the full trace is available (execution complete). */
  onComplete(traceJson: string): void;
  /** Called when the server reports an error. */
  onError(message: string): void;
}

export interface LiveConnection {
  /** Send a step command (execute one tick then pause). */
  step(): void;
  /** Send a resume command (continue executing). */
  resume(): void;
  /** Send a pause command (stop auto-stepping). */
  pause(): void;
  /** Send a cancel command (cancel execution). */
  cancel(): void;
  /** Disconnect from the server. */
  disconnect(): void;
  /** Current connection status. */
  status: LiveStatus;
}

/** Accumulated node states from live events. */
export function applyDebugEvents(
  current: Map<string, NodeState>,
  events: DebugEvent[]
): Map<string, NodeState> {
  const updated = new Map(current);
  for (const ev of events) {
    // Use the to_state as the current state, but only for state change events
    const toState = ev.to_state as NodeState;
    if (toState) {
      updated.set(ev.node_id, toState);
    }
  }
  return updated;
}

/** Connect to a psflow debug server via WebSocket. */
export function connectLive(url: string, callbacks: LiveCallbacks): LiveConnection {
  let status: LiveStatus = "connecting";
  let ws: WebSocket | null = null;

  function send(msg: { command: string }): void {
    if (ws?.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify(msg));
    }
  }

  function setStatus(s: LiveStatus): void {
    status = s;
    callbacks.onStatusChange(s);
  }

  try {
    ws = new WebSocket(url);
  } catch {
    setStatus("disconnected");
    return makeConnection();
  }

  ws.onopen = () => {
    // Status will be set by first server message (paused)
  };

  ws.onmessage = (event) => {
    let msg: ServerMsg;
    try {
      msg = JSON.parse(event.data);
    } catch {
      return;
    }

    switch (msg.type) {
      case "graph":
        callbacks.onGraph(msg.source);
        break;
      case "events":
        callbacks.onEvents(msg.events);
        break;
      case "paused":
        setStatus("paused");
        break;
      case "resumed":
        setStatus("running");
        break;
      case "complete":
        setStatus("complete");
        callbacks.onComplete(msg.trace_json);
        break;
      case "error":
        callbacks.onError(msg.message);
        setStatus("disconnected");
        break;
    }
  };

  ws.onclose = () => {
    ws = null;
    if (status !== "complete") {
      setStatus("disconnected");
    }
  };

  ws.onerror = () => {
    // onclose will follow
  };

  function makeConnection(): LiveConnection {
    return {
      step() { send({ command: "step" }); },
      resume() { send({ command: "resume" }); },
      pause() { send({ command: "pause" }); },
      cancel() { send({ command: "cancel" }); },
      disconnect() {
        ws?.close();
        ws = null;
        setStatus("disconnected");
      },
      get status() { return status; },
    };
  }

  return makeConnection();
}
