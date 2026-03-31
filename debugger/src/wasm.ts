import init, { parse_mmd, parse_trace } from "../pkg/psflow_wasm.js";
import type { ParseResult, TraceResult } from "../pkg/psflow_wasm.js";

let initialized = false;

export async function initWasm(): Promise<void> {
  if (!initialized) {
    await init();
    initialized = true;
  }
}

export { parse_mmd, parse_trace };
export type { ParseResult, TraceResult };
