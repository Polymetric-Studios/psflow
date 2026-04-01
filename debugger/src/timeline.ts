import type { DebuggerState } from "./state.js";
import type { PlaybackController } from "./playback.js";

const STATE_COLORS: Record<string, string> = {
  pending: "var(--state-pending-border)",
  running: "var(--state-running-border)",
  completed: "var(--state-completed-border)",
  failed: "var(--state-failed-border)",
  cancelled: "var(--state-cancelled-border)",
};

interface TimelineState {
  /** Zoom level: 1 = fit all events, >1 = zoomed in */
  zoom: number;
  /** Scroll offset as fraction (0–1) of the zoomed content */
  scrollOffset: number;
  /** Whether the user is currently dragging the scrubber */
  dragging: boolean;
}

let tl: TimelineState = { zoom: 1, scrollOffset: 0, dragging: false };
let bar: HTMLElement;
let tickLayer: HTMLElement;
let playhead: HTMLElement;
let playbackRef: PlaybackController;
let stateRef: DebuggerState;
let cleanupFns: (() => void)[] = [];

export function destroyTimeline(): void {
  for (const fn of cleanupFns) fn();
  cleanupFns = [];
}

export function initTimeline(
  state: DebuggerState,
  playback: PlaybackController
): void {
  // Clean up any previous listeners (e.g. on hot reload)
  destroyTimeline();

  stateRef = state;
  playbackRef = playback;

  const pane = document.getElementById("timeline-pane")!;
  pane.innerHTML = `
    <div id="timeline-controls">
      <button id="tl-zoom-in" title="Zoom in (+)">+</button>
      <button id="tl-zoom-out" title="Zoom out (-)">-</button>
      <button id="tl-zoom-fit" title="Fit all">Fit</button>
    </div>
    <div id="timeline-bar">
      <div id="timeline-progress"></div>
      <div id="timeline-ticks"></div>
      <div id="timeline-playhead"></div>
    </div>
  `;

  bar = document.getElementById("timeline-bar")!;
  tickLayer = document.getElementById("timeline-ticks")!;
  playhead = document.getElementById("timeline-playhead")!;

  // Click/drag to seek
  bar.addEventListener("mousedown", onMouseDown);
  document.addEventListener("mousemove", onMouseMove);
  document.addEventListener("mouseup", onMouseUp);

  // Zoom with mouse wheel
  bar.addEventListener("wheel", onWheel, { passive: false });

  // Register cleanup for document-level listeners
  cleanupFns.push(
    () => document.removeEventListener("mousemove", onMouseMove),
    () => document.removeEventListener("mouseup", onMouseUp),
  );

  // Zoom buttons
  document.getElementById("tl-zoom-in")!.addEventListener("click", () => applyZoom(tl.zoom * 1.5));
  document.getElementById("tl-zoom-out")!.addEventListener("click", () => applyZoom(tl.zoom / 1.5));
  document.getElementById("tl-zoom-fit")!.addEventListener("click", () => applyZoom(1));
}

function positionFromEvent(e: MouseEvent): number {
  if (!stateRef.trace || stateRef.trace.events.length === 0) return -1;
  const rect = bar.getBoundingClientRect();
  const barRatio = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
  // Convert bar-space ratio to content-space ratio accounting for zoom + scroll
  const contentRatio = tl.scrollOffset + barRatio / tl.zoom;
  return Math.round(Math.max(0, Math.min(1, contentRatio)) * (stateRef.trace.events.length - 1));
}

function onMouseDown(e: MouseEvent): void {
  if (!stateRef.trace) return;
  tl.dragging = true;
  const pos = positionFromEvent(e);
  if (pos >= 0) playbackRef.seekTo(pos);
  e.preventDefault();
}

function onMouseMove(e: MouseEvent): void {
  if (!tl.dragging) return;
  const pos = positionFromEvent(e);
  if (pos >= 0) playbackRef.seekTo(pos);
}

function onMouseUp(): void {
  tl.dragging = false;
}

function onWheel(e: WheelEvent): void {
  e.preventDefault();
  if (!stateRef.trace) return;

  if (e.ctrlKey || e.metaKey) {
    // Zoom
    const factor = e.deltaY < 0 ? 1.25 : 0.8;
    const rect = bar.getBoundingClientRect();
    const mouseRatio = (e.clientX - rect.left) / rect.width;
    zoomAt(factor, mouseRatio);
  } else {
    // Pan when zoomed
    if (tl.zoom <= 1) return;
    const panAmount = (e.deltaX || e.deltaY) * 0.001;
    tl.scrollOffset = clampScroll(tl.scrollOffset + panAmount);
    renderTimeline();
  }
}

function zoomAt(factor: number, anchorRatio: number): void {
  const oldZoom = tl.zoom;
  const newZoom = Math.max(1, Math.min(50, oldZoom * factor));
  // Keep the content under the cursor stationary
  const contentAtAnchor = tl.scrollOffset + anchorRatio / oldZoom;
  tl.zoom = newZoom;
  tl.scrollOffset = clampScroll(contentAtAnchor - anchorRatio / newZoom);
  renderTimeline();
}

function applyZoom(newZoom: number): void {
  tl.zoom = Math.max(1, Math.min(50, newZoom));
  tl.scrollOffset = clampScroll(tl.scrollOffset);
  renderTimeline();
}

function clampScroll(offset: number): number {
  const maxScroll = Math.max(0, 1 - 1 / tl.zoom);
  return Math.max(0, Math.min(maxScroll, offset));
}

/** Convert a content-space ratio [0..1] to a bar-space pixel percentage. */
function contentToBar(contentRatio: number): number {
  return ((contentRatio - tl.scrollOffset) * tl.zoom) * 100;
}

function renderTicks(): void {
  if (!stateRef.trace || stateRef.trace.events.length === 0) {
    tickLayer.innerHTML = "";
    return;
  }

  const events = stateRef.trace.events;
  const total = events.length;

  // Determine tick density — skip ticks if too dense at current zoom
  const barWidth = bar.getBoundingClientRect().width || 800;
  const visibleEvents = total / tl.zoom;
  const minPxPerTick = 3;
  const maxTicks = barWidth / minPxPerTick;
  const step = Math.max(1, Math.ceil(visibleEvents / maxTicks));

  let html = "";
  for (let i = 0; i < total; i += step) {
    const ratio = total === 1 ? 0.5 : i / (total - 1);
    const left = contentToBar(ratio);
    if (left < -1 || left > 101) continue; // Off-screen
    const color = STATE_COLORS[events[i].state] ?? "var(--text-dim)";
    html += `<span class="timeline-tick" style="left:${left.toFixed(2)}%;background:${color}"></span>`;
  }
  tickLayer.innerHTML = html;
}

function renderPlayhead(): void {
  if (!stateRef.trace || stateRef.trace.events.length === 0 || stateRef.tracePosition < 0) {
    playhead.style.display = "none";
    return;
  }
  const total = stateRef.trace.events.length;
  const ratio = total === 1 ? 0.5 : stateRef.tracePosition / (total - 1);
  const left = contentToBar(ratio);

  if (left < -1 || left > 101) {
    playhead.style.display = "none";
  } else {
    playhead.style.display = "";
    playhead.style.left = `${left.toFixed(2)}%`;
  }
}

let lastZoom = -1;
let lastScroll = -1;
let lastEventCount = -1;

function renderTimeline(): void {
  // Progress bar
  const progress = document.getElementById("timeline-progress");
  if (progress && stateRef.trace && stateRef.trace.events.length > 0) {
    const ratio = (stateRef.tracePosition + 1) / stateRef.trace.events.length;
    const left = contentToBar(0);
    const right = contentToBar(ratio);
    progress.style.left = `${Math.max(0, left).toFixed(2)}%`;
    progress.style.width = `${Math.max(0, right - Math.max(0, left)).toFixed(2)}%`;
  }

  // Only rebuild ticks when zoom, scroll, or event count changes
  const eventCount = stateRef.trace?.events.length ?? 0;
  if (tl.zoom !== lastZoom || tl.scrollOffset !== lastScroll || eventCount !== lastEventCount) {
    lastZoom = tl.zoom;
    lastScroll = tl.scrollOffset;
    lastEventCount = eventCount;
    renderTicks();
  }

  renderPlayhead();
}

export function updateTimeline(state: DebuggerState): void {
  stateRef = state;
  renderTimeline();

  // Auto-scroll to keep playhead visible when zoomed
  if (tl.zoom > 1 && state.trace && state.trace.events.length > 0 && state.tracePosition >= 0 && state.playing) {
    const total = state.trace.events.length;
    const ratio = total === 1 ? 0.5 : state.tracePosition / (total - 1);
    const barPos = contentToBar(ratio);
    if (barPos < 5 || barPos > 95) {
      tl.scrollOffset = clampScroll(ratio - 0.5 / tl.zoom);
      renderTimeline();
    }
  }
}