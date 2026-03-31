import type { DebuggerState } from "./state.js";
import type { PlaybackController } from "./playback.js";

export function initTimeline(
  state: DebuggerState,
  playback: PlaybackController
): void {
  const pane = document.getElementById("timeline-pane")!;
  pane.innerHTML = `<div id="timeline-bar"><div id="timeline-progress"></div></div>`;

  const bar = document.getElementById("timeline-bar")!;
  bar.addEventListener("click", (e) => {
    if (!state.trace) return;
    const rect = bar.getBoundingClientRect();
    const ratio = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
    const position = Math.round(ratio * (state.trace.events.length - 1));
    playback.seekTo(position);
  });
}

export function updateTimeline(state: DebuggerState): void {
  const progress = document.getElementById("timeline-progress");
  if (!progress || !state.trace || state.trace.events.length === 0) return;

  const ratio = (state.tracePosition + 1) / state.trace.events.length;
  progress.style.width = `${(ratio * 100).toFixed(1)}%`;
}
