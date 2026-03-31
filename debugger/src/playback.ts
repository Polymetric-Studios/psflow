import type { DebuggerState } from "./state.js";
import { deriveNodeStates } from "./state.js";

export interface PlaybackController {
  stepForward(): void;
  stepBack(): void;
  play(): void;
  pause(): void;
  toggle(): void;
  seekTo(position: number): void;
  setSpeed(speed: number): void;
  destroy(): void;
}

export function createPlayback(
  state: DebuggerState,
  onUpdate: () => void
): PlaybackController {
  let timerId: ReturnType<typeof setTimeout> | null = null;

  function updateStates(): void {
    if (state.trace) {
      state.nodeStates = deriveNodeStates(state.trace, state.tracePosition);
    }
    onUpdate();
  }

  function stepForward(): void {
    if (!state.trace) return;
    if (state.tracePosition < state.trace.events.length - 1) {
      state.tracePosition++;
      updateStates();

      // Check breakpoints
      const ev = state.trace.events[state.tracePosition];
      if (state.playing && ev && state.breakpoints.has(ev.node_id) && ev.state === "running") {
        pause();
      }
    } else {
      pause();
    }
  }

  function stepBack(): void {
    if (!state.trace) return;
    // Position -1 is the "before start" sentinel — all nodes idle.
    // Allow stepping back to -1 but not beyond.
    if (state.tracePosition >= 0) {
      state.tracePosition--;
      updateStates();
    }
  }

  function scheduleNext(): void {
    if (!state.playing || !state.trace) return;
    // Base interval: simulate timing from trace, or fixed interval
    const baseMs = 300;
    const delay = Math.max(30, baseMs / state.speed);
    timerId = setTimeout(() => {
      stepForward();
      if (state.playing) scheduleNext();
    }, delay);
  }

  function play(): void {
    if (!state.trace) return;
    state.playing = true;
    updateStates();
    scheduleNext();
  }

  function pause(): void {
    state.playing = false;
    if (timerId !== null) {
      clearTimeout(timerId);
      timerId = null;
    }
    updateStates();
  }

  function toggle(): void {
    if (state.playing) pause();
    else play();
  }

  function seekTo(position: number): void {
    if (!state.trace) return;
    state.tracePosition = Math.max(-1, Math.min(position, state.trace.events.length - 1));
    updateStates();
  }

  function setSpeed(speed: number): void {
    state.speed = speed;
  }

  function destroy(): void {
    pause();
  }

  return { stepForward, stepBack, play, pause, toggle, seekTo, setSpeed, destroy };
}
