import { describe, it, expect, vi } from "vitest";

// Mock the WASM module before importing state.ts
vi.mock("../pkg/psflow_wasm.js", () => ({}));

// Mock localStorage for createState/loadBreakpoints
vi.stubGlobal("localStorage", {
  getItem: () => null,
  setItem: () => {},
});

import { deriveNodeStates, getNodeEvent } from "./state.js";

// --- Test helpers ---

interface MockTraceEvent {
  node_id: string;
  state: string;
  order: number;
  elapsed_ms: number | undefined;
  error: string | undefined;
  outputs_json: string | undefined;
}

function makeEvent(
  nodeId: string,
  state: string,
  order: number,
): MockTraceEvent {
  return {
    node_id: nodeId,
    state,
    order,
    elapsed_ms: undefined,
    error: undefined,
    outputs_json: undefined,
  };
}

function makeTrace(events: MockTraceEvent[]) {
  return {
    events,
    total_elapsed_ms: 0,
  } as any; // Cast to TraceResult shape
}

// --- Tests ---

describe("deriveNodeStates", () => {
  it("at position -1 returns empty map", () => {
    const trace = makeTrace([
      makeEvent("A", "pending", 0),
      makeEvent("A", "running", 1),
    ]);

    const result = deriveNodeStates(trace, -1);
    expect(result.size).toBe(0);
  });

  it("accumulates states up to position", () => {
    const trace = makeTrace([
      makeEvent("A", "pending", 0),
      makeEvent("A", "running", 1),
      makeEvent("A", "completed", 2),
      makeEvent("B", "pending", 3),
      makeEvent("B", "running", 4),
    ]);

    // Position 2 means events 0, 1, 2 are applied (A:completed)
    const result = deriveNodeStates(trace, 2);

    expect(result.get("A")).toBe("completed");
    expect(result.has("B")).toBe(false);
  });

  it("at last position has all states", () => {
    const trace = makeTrace([
      makeEvent("A", "pending", 0),
      makeEvent("A", "running", 1),
      makeEvent("A", "completed", 2),
      makeEvent("B", "pending", 3),
      makeEvent("B", "running", 4),
    ]);

    const result = deriveNodeStates(trace, 4);

    expect(result.get("A")).toBe("completed");
    expect(result.get("B")).toBe("running");
    expect(result.size).toBe(2);
  });

  it("overwrites earlier states for the same node", () => {
    const trace = makeTrace([
      makeEvent("A", "pending", 0),
      makeEvent("A", "running", 1),
    ]);

    const result = deriveNodeStates(trace, 1);
    expect(result.get("A")).toBe("running");
    expect(result.size).toBe(1);
  });
});

describe("getNodeEvent", () => {
  it("returns latest event for node at position", () => {
    const trace = makeTrace([
      makeEvent("A", "pending", 0),
      makeEvent("B", "pending", 1),
      makeEvent("A", "running", 2),
      makeEvent("B", "running", 3),
      makeEvent("A", "completed", 4),
    ]);

    const event = getNodeEvent(trace, 3, "A");
    expect(event).not.toBeNull();
    expect(event!.node_id).toBe("A");
    expect(event!.state).toBe("running");
  });

  it("returns null when node has no events up to position", () => {
    const trace = makeTrace([
      makeEvent("A", "pending", 0),
      makeEvent("A", "running", 1),
    ]);

    const event = getNodeEvent(trace, 1, "B");
    expect(event).toBeNull();
  });

  it("returns the first event when position is 0", () => {
    const trace = makeTrace([
      makeEvent("A", "pending", 0),
      makeEvent("A", "running", 1),
    ]);

    const event = getNodeEvent(trace, 0, "A");
    expect(event).not.toBeNull();
    expect(event!.state).toBe("pending");
  });
});
