import { describe, it, expect, vi } from "vitest";

// Mock the WASM module (transitive dependency via state.ts)
vi.mock("../pkg/psflow_wasm.js", () => ({}));

import { applyDebugEvents } from "./live.js";
import type { NodeState } from "./state.js";

// --- Test helpers ---

interface MockDebugEvent {
  node_id: string;
  from_state: string;
  to_state: string;
  elapsed_ms?: number;
  outputs_json?: string;
  error?: string;
}

function makeDebugEvent(
  nodeId: string,
  fromState: string,
  toState: string,
): MockDebugEvent {
  return {
    node_id: nodeId,
    from_state: fromState,
    to_state: toState,
  };
}

// --- Tests ---

describe("applyDebugEvents", () => {
  it("on empty map adds new states", () => {
    const current = new Map<string, NodeState>();
    const events = [
      makeDebugEvent("A", "idle", "pending"),
      makeDebugEvent("B", "idle", "running"),
    ];

    const result = applyDebugEvents(current, events as any);

    expect(result.get("A")).toBe("pending");
    expect(result.get("B")).toBe("running");
    expect(result.size).toBe(2);
  });

  it("updates existing states", () => {
    const current = new Map<string, NodeState>([
      ["A", "pending"],
      ["B", "running"],
    ]);
    const events = [
      makeDebugEvent("A", "pending", "running"),
      makeDebugEvent("B", "running", "completed"),
    ];

    const result = applyDebugEvents(current, events as any);

    expect(result.get("A")).toBe("running");
    expect(result.get("B")).toBe("completed");
  });

  it("preserves unaffected nodes", () => {
    const current = new Map<string, NodeState>([
      ["A", "completed"],
      ["B", "running"],
      ["C", "pending"],
    ]);
    const events = [
      makeDebugEvent("B", "running", "completed"),
    ];

    const result = applyDebugEvents(current, events as any);

    expect(result.get("A")).toBe("completed");
    expect(result.get("B")).toBe("completed");
    expect(result.get("C")).toBe("pending");
    expect(result.size).toBe(3);
  });

  it("handles empty events array", () => {
    const current = new Map<string, NodeState>([
      ["A", "running"],
      ["B", "pending"],
    ]);

    const result = applyDebugEvents(current, []);

    expect(result.get("A")).toBe("running");
    expect(result.get("B")).toBe("pending");
    expect(result.size).toBe(2);
  });

  it("does not mutate the original map", () => {
    const current = new Map<string, NodeState>([["A", "pending"]]);
    const events = [makeDebugEvent("A", "pending", "running")];

    const result = applyDebugEvents(current, events as any);

    expect(current.get("A")).toBe("pending");
    expect(result.get("A")).toBe("running");
  });
});
