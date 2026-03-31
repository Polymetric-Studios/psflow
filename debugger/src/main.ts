import { initWasm, parse_mmd, parse_trace } from "./wasm.js";
import { createEditor, type EditorHandle } from "./editor.js";
import { createState, type DebuggerState } from "./state.js";
import { renderInspector } from "./inspector.js";
import { createPlayback, type PlaybackController } from "./playback.js";
import { initTimeline, updateTimeline } from "./timeline.js";

// --- Global state ---

const state: DebuggerState = createState();
let editor: EditorHandle;
let playback: PlaybackController;

// --- UI update ---

function update(): void {
  editor.updateNodeStates(state.nodeStates);
  editor.selectNode(state.selectedNodeId);
  renderInspector(state);
  updateTimeline(state);
  updateToolbar();

  // Auto-scroll to running node during playback
  if (state.playing && state.trace && state.tracePosition >= 0) {
    const ev = state.trace.events[state.tracePosition];
    if (ev) editor.scrollToNode(ev.node_id);
  }
}

function updateToolbar(): void {
  const btnPlay = document.getElementById("btn-play") as HTMLButtonElement;
  const stepCounter = document.getElementById("step-counter")!;
  const status = document.getElementById("status")!;

  btnPlay.textContent = state.playing ? "Pause" : "Play";

  if (state.trace) {
    stepCounter.textContent = `${state.tracePosition + 1} / ${state.trace.events.length}`;
  }

  if (state.source && state.parseResult) {
    const nodeCount = state.parseResult.nodes.length;
    status.textContent = state.trace
      ? `${nodeCount} nodes | trace loaded`
      : `${nodeCount} nodes`;
  }
}

// --- File loading ---

function setSource(text: string, fileName: string): void {
  state.source = text;
  state.parseResult = parse_mmd(text);
  state.trace = null;
  state.tracePosition = -1;
  state.nodeStates = new Map();
  state.selectedNodeId = null;
  state.playing = false;

  editor.setSource(text);
  editor.updateParseResult(state.parseResult);

  document.getElementById("file-name")!.textContent = fileName;
  (document.getElementById("btn-load-trace") as HTMLButtonElement).disabled = false;
  (document.getElementById("btn-run") as HTMLButtonElement).disabled = false;

  if (state.parseResult.errors.length > 0) {
    document.getElementById("status")!.textContent =
      `Parse errors: ${state.parseResult.errors.length}`;
  }

  update();
}

async function loadMmdFile(file: File): Promise<void> {
  setSource(await file.text(), file.name);
}

function loadTrace(traceData: import("../pkg/psflow_wasm.js").TraceResult): void {
  state.trace = traceData;
  state.tracePosition = -1;
  state.nodeStates = new Map();
  state.playing = false;

  for (const id of ["btn-step-back", "btn-play", "btn-step-fwd", "speed-select"]) {
    (document.getElementById(id) as HTMLButtonElement).disabled = false;
  }

  update();
}

async function loadTraceFile(file: File): Promise<void> {
  loadTrace(parse_trace(await file.text()));
}

// --- Run graph ---

async function runGraph(): Promise<void> {
  if (!state.source) return;

  const status = document.getElementById("status")!;
  const btnRun = document.getElementById("btn-run") as HTMLButtonElement;
  btnRun.disabled = true;
  status.textContent = "Running...";

  try {
    const resp = await fetch("/api/run", {
      method: "POST",
      body: state.source,
    });
    const data = await resp.json();

    if (data.error) {
      status.textContent = `Run failed`;
      // Show the error in the inspector
      document.getElementById("inspector-content")!.innerHTML =
        `<div class="inspector-section"><h3>Error</h3><pre class="inspector-json">${escapeHtml(data.error)}</pre></div>`;
      return;
    }

    // Parse and load the trace
    const traceJson = JSON.stringify(data.trace);
    loadTrace(parse_trace(traceJson));

    // Auto-play to the end so you see the result immediately
    playback.seekTo(state.trace!.events.length - 1);

    status.textContent = `${state.parseResult!.nodes.length} nodes | executed in ${data.trace.elapsed.secs * 1000 + data.trace.elapsed.nanos / 1e6 | 0}ms`;
  } finally {
    btnRun.disabled = false;
  }
}

function escapeHtml(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

// --- Keyboard shortcuts ---

function handleKeyboard(e: KeyboardEvent): void {
  // Don't capture when focused on input elements
  if (e.target instanceof HTMLInputElement || e.target instanceof HTMLSelectElement) return;

  if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    runGraph().catch(showError);
    return;
  }

  switch (e.key) {
    case " ":
      e.preventDefault();
      playback.toggle();
      break;
    case "ArrowRight":
      e.preventDefault();
      playback.pause();
      playback.stepForward();
      break;
    case "ArrowLeft":
      e.preventDefault();
      playback.pause();
      playback.stepBack();
      break;
    case "+":
    case "=":
      e.preventDefault();
      cycleSpeed(1);
      break;
    case "-":
      e.preventDefault();
      cycleSpeed(-1);
      break;
  }
}

function cycleSpeed(direction: number): void {
  const select = document.getElementById("speed-select") as HTMLSelectElement;
  const newIndex = select.selectedIndex + direction;
  if (newIndex >= 0 && newIndex < select.options.length) {
    select.selectedIndex = newIndex;
    playback.setSpeed(parseFloat(select.value));
  }
}

function showError(err: unknown): void {
  const msg = err instanceof Error ? err.message : String(err);
  console.error("psflow-debugger:", err);
  document.getElementById("status")!.textContent = `Error: ${msg}`;
}

// --- Init ---

async function main(): Promise<void> {
  await initWasm();

  // Create editor
  editor = createEditor(
    document.getElementById("editor-pane")!,
    (nodeId) => {
      state.selectedNodeId = nodeId;
      update();
    }
  );

  // Create playback controller
  playback = createPlayback(state, update);

  // Init timeline
  initTimeline(state, playback);

  // Wire up buttons
  document.getElementById("btn-load-mmd")!.addEventListener("click", () => {
    document.getElementById("file-input-mmd")!.click();
  });

  document.getElementById("file-input-mmd")!.addEventListener("change", (e) => {
    const file = (e.target as HTMLInputElement).files?.[0];
    if (file) loadMmdFile(file).catch(showError);
  });

  document.getElementById("btn-load-trace")!.addEventListener("click", () => {
    document.getElementById("file-input-trace")!.click();
  });

  document.getElementById("file-input-trace")!.addEventListener("change", (e) => {
    const file = (e.target as HTMLInputElement).files?.[0];
    if (file) loadTraceFile(file).catch(showError);
  });

  document.getElementById("btn-run")!.addEventListener("click", () => runGraph().catch(showError));

  document.getElementById("btn-play")!.addEventListener("click", () => playback.toggle());
  document.getElementById("btn-step-fwd")!.addEventListener("click", () => {
    playback.pause();
    playback.stepForward();
  });
  document.getElementById("btn-step-back")!.addEventListener("click", () => {
    playback.pause();
    playback.stepBack();
  });

  document.getElementById("speed-select")!.addEventListener("change", (e) => {
    playback.setSpeed(parseFloat((e.target as HTMLSelectElement).value));
  });

  // Keyboard shortcuts
  document.addEventListener("keydown", handleKeyboard);

  document.getElementById("status")!.textContent = "Ready";
}

main().catch(showError);
