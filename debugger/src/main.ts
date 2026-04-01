import { initWasm, parse_mmd, parse_trace } from "./wasm.js";
import { createEditor, type EditorHandle } from "./editor.js";
import { createState, saveBreakpoints, type DebuggerState } from "./state.js";
import { renderInspector, setInspectorOnUpdate } from "./inspector.js";
import { createPlayback, type PlaybackController } from "./playback.js";
import { initTimeline, updateTimeline } from "./timeline.js";
import { connectLive, applyDebugEvents, type LiveConnection, type LiveStatus } from "./live.js";
import { createGraph, type GraphHandle } from "./graph.js";

// --- Global state ---

type ViewMode = "text" | "split" | "graph";

const state: DebuggerState = createState();
let editor: EditorHandle;
let graph: GraphHandle;
let playback: PlaybackController;
let liveConn: LiveConnection | null = null;

// --- UI update ---

function update(): void {
  editor.updateNodeStates(state.nodeStates);
  editor.updateTrace(state.trace, state.tracePosition);
  editor.selectNode(state.selectedNodeId);
  editor.updateBreakpoints(state.breakpoints);
  graph.updateNodeStates(state.nodeStates);
  graph.selectNode(state.selectedNodeId);
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

  // Toggle play/pause icons
  const playIcon = btnPlay.querySelector(".play-icon") as SVGElement | null;
  const pauseIcon = btnPlay.querySelector(".pause-icon") as SVGElement | null;
  if (playIcon && pauseIcon) {
    playIcon.style.display = state.playing ? "none" : "";
    pauseIcon.style.display = state.playing ? "" : "none";
  }

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

async function setSource(text: string, fileName: string): Promise<void> {
  state.source = text;
  state.parseResult = parse_mmd(text);
  state.trace = null;
  state.tracePosition = -1;
  state.nodeStates = new Map();
  state.selectedNodeId = null;
  state.playing = false;

  editor.setSource(text);
  editor.updateParseResult(state.parseResult);
  await graph.setGraph(state.parseResult);

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
  await setSource(await file.text(), file.name);
}

function loadTrace(traceData: import("../pkg/psflow_wasm.js").TraceResult): void {
  state.trace = traceData;
  state.tracePosition = -1;
  state.nodeStates = new Map();
  state.playing = false;

  for (const id of ["btn-step-back", "btn-play", "btn-step-fwd", "btn-reset", "speed-select"]) {
    (document.getElementById(id) as HTMLButtonElement).disabled = false;
  }

  update();
}

async function loadTraceFile(file: File): Promise<void> {
  loadTrace(parse_trace(await file.text()));
}

function resetTrace(): void {
  playback.pause();
  state.trace = null;
  state.tracePosition = -1;
  state.nodeStates = new Map();
  state.selectedNodeId = null;

  for (const id of ["btn-step-back", "btn-play", "btn-step-fwd", "btn-reset", "speed-select"]) {
    (document.getElementById(id) as HTMLButtonElement).disabled = true;
  }
  document.getElementById("step-counter")!.textContent = "";

  update();
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

// --- Live connection ---

function startLiveConnection(url: string): void {
  if (liveConn) liveConn.disconnect();

  // Reset state for live mode
  state.trace = null;
  state.tracePosition = -1;
  state.nodeStates = new Map();
  state.playing = false;

  liveConn = connectLive(url, {
    onGraph(source) {
      setSource(source, "live").catch(showError);
      updateLiveUI("paused");
    },

    onEvents(events) {
      // Apply events to node states
      state.nodeStates = applyDebugEvents(state.nodeStates, events);
      update();

      // Auto-scroll to the latest running node
      const running = events.find(e => e.to_state === "running");
      if (running) {
        editor.scrollToNode(running.node_id);
      }
    },

    onStatusChange(status) {
      updateLiveUI(status);
    },

    onComplete(traceJson) {
      // Load the final trace for replay
      const trace = parse_trace(traceJson);
      loadTrace(trace);
      playback.seekTo(state.trace!.events.length - 1);
      updateLiveUI("complete");
    },

    onError(message) {
      document.getElementById("status")!.textContent = `Live error: ${message}`;
    },
  });
}

function updateLiveUI(liveStatus: LiveStatus): void {
  const statusEl = document.getElementById("status")!;
  const indicator = document.getElementById("live-indicator")!;
  const btnConnect = document.getElementById("btn-connect") as HTMLButtonElement;
  const btnLiveStep = document.getElementById("btn-live-step") as HTMLButtonElement;
  const btnLiveResume = document.getElementById("btn-live-resume") as HTMLButtonElement;
  const btnLivePause = document.getElementById("btn-live-pause") as HTMLButtonElement;
  const liveControls = document.getElementById("live-controls")!;
  const portInput = document.getElementById("ws-port") as HTMLInputElement;

  indicator.className = "live-indicator";

  switch (liveStatus) {
    case "connecting":
      statusEl.textContent = "Connecting...";
      indicator.classList.add("connecting");
      btnConnect.textContent = "Disconnect";
      btnConnect.classList.add("connected");
      portInput.disabled = true;
      liveControls.style.display = "none";
      break;
    case "paused":
      statusEl.textContent = "Paused";
      indicator.classList.add("connected");
      btnConnect.textContent = "Disconnect";
      btnConnect.classList.add("connected");
      portInput.disabled = true;
      liveControls.style.display = "flex";
      btnLiveStep.disabled = false;
      btnLiveResume.disabled = false;
      btnLivePause.disabled = true;
      break;
    case "running":
      statusEl.textContent = "Running";
      indicator.classList.add("connected");
      btnConnect.textContent = "Disconnect";
      btnConnect.classList.add("connected");
      portInput.disabled = true;
      liveControls.style.display = "flex";
      btnLiveStep.disabled = true;
      btnLiveResume.disabled = true;
      btnLivePause.disabled = false;
      break;
    case "complete":
      statusEl.textContent = "Trace loaded";
      btnConnect.textContent = "Connect";
      btnConnect.classList.remove("connected");
      portInput.disabled = false;
      liveControls.style.display = "none";
      liveConn = null;
      break;
    case "disconnected":
      statusEl.textContent = "Connection failed";
      btnConnect.textContent = "Connect";
      btnConnect.classList.remove("connected");
      portInput.disabled = false;
      liveControls.style.display = "none";
      liveConn = null;
      break;
  }
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

// --- View mode ---

function setViewMode(mode: ViewMode): void {
  const editorPane = document.getElementById("editor-pane")!;
  const graphPane = document.getElementById("graph-pane")!;
  const graphHandle = document.getElementById("graph-resize-handle")!;

  const btnText = document.getElementById("btn-view-text")!;
  const btnSplit = document.getElementById("btn-view-split")!;
  const btnGraph = document.getElementById("btn-view-graph")!;
  btnText.classList.toggle("active", mode === "text");
  btnSplit.classList.toggle("active", mode === "split");
  btnGraph.classList.toggle("active", mode === "graph");

  switch (mode) {
    case "text":
      editorPane.style.display = "";
      graphPane.style.display = "none";
      graphHandle.style.display = "none";
      break;
    case "graph":
      editorPane.style.display = "none";
      graphPane.style.display = "flex";
      graphHandle.style.display = "none";
      break;
    case "split":
      editorPane.style.display = "";
      graphPane.style.display = "flex";
      graphHandle.style.display = "";
      break;
  }

  // Persist preference
  localStorage.setItem("psflow-view-mode", mode);
}

// --- Graph pane resize handle ---

function initGraphResizeHandle(): void {
  const handle = document.getElementById("graph-resize-handle")!;
  const editorPane = document.getElementById("editor-pane")!;
  const mainEl = document.getElementById("main")!;
  let startX = 0;
  let startWidth = 0;

  function onMouseDown(e: MouseEvent): void {
    startX = e.clientX;
    startWidth = editorPane.offsetWidth;
    handle.classList.add("dragging");
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    document.addEventListener("mousemove", onMouseMove);
    document.addEventListener("mouseup", onMouseUp);
    e.preventDefault();
  }

  function onMouseMove(e: MouseEvent): void {
    const delta = e.clientX - startX;
    const mainWidth = mainEl.offsetWidth;
    const newWidth = Math.max(200, Math.min(mainWidth - 400, startWidth + delta));
    editorPane.style.flex = "none";
    editorPane.style.width = `${newWidth}px`;
  }

  function onMouseUp(): void {
    handle.classList.remove("dragging");
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    document.removeEventListener("mousemove", onMouseMove);
    document.removeEventListener("mouseup", onMouseUp);
  }

  handle.addEventListener("mousedown", onMouseDown);
}

// --- Resize handle ---

function initResizeHandle(): void {
  const handle = document.getElementById("resize-handle")!;
  const inspector = document.getElementById("inspector-pane")!;
  let startX = 0;
  let startWidth = 0;

  function onMouseDown(e: MouseEvent): void {
    startX = e.clientX;
    startWidth = inspector.offsetWidth;
    handle.classList.add("dragging");
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
    document.addEventListener("mousemove", onMouseMove);
    document.addEventListener("mouseup", onMouseUp);
    e.preventDefault();
  }

  function onMouseMove(e: MouseEvent): void {
    const delta = startX - e.clientX;
    const newWidth = Math.max(200, Math.min(600, startWidth + delta));
    inspector.style.width = `${newWidth}px`;
  }

  function onMouseUp(): void {
    handle.classList.remove("dragging");
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    document.removeEventListener("mousemove", onMouseMove);
    document.removeEventListener("mouseup", onMouseUp);
  }

  handle.addEventListener("mousedown", onMouseDown);
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
    },
    (nodeId) => {
      // Toggle breakpoint
      if (state.breakpoints.has(nodeId)) {
        state.breakpoints.delete(nodeId);
      } else {
        state.breakpoints.add(nodeId);
      }
      saveBreakpoints(state.breakpoints);
      update();
    },
  );

  // Create graph view
  graph = createGraph(
    document.getElementById("graph-pane")!,
    (nodeId) => {
      state.selectedNodeId = nodeId;
      update();
    },
    (nodeId) => {
      // Double-click: switch to text view and scroll to node
      setViewMode("text");
      editor.scrollToNode(nodeId);
    },
  );

  // Create playback controller
  playback = createPlayback(state, update);

  // Inspector update callback (for breakpoint list click → re-render)
  setInspectorOnUpdate(update);

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

  document.getElementById("btn-reset")!.addEventListener("click", () => resetTrace());

  document.getElementById("speed-select")!.addEventListener("change", (e) => {
    playback.setSpeed(parseFloat((e.target as HTMLSelectElement).value));
  });

  // Live connection
  document.getElementById("btn-connect")!.addEventListener("click", () => {
    if (liveConn) {
      liveConn.disconnect();
    } else {
      const portStr = (document.getElementById("ws-port") as HTMLInputElement).value || "9001";
      const port = parseInt(portStr, 10);
      if (isNaN(port) || port < 1 || port > 65535) {
        document.getElementById("status")!.textContent = "Invalid port";
        return;
      }
      startLiveConnection(`ws://127.0.0.1:${port}`);
    }
  });
  document.getElementById("btn-live-step")!.addEventListener("click", () => liveConn?.step());
  document.getElementById("btn-live-resume")!.addEventListener("click", () => liveConn?.resume());
  document.getElementById("btn-live-pause")!.addEventListener("click", () => liveConn?.pause());

  // View toggle buttons
  document.getElementById("btn-view-text")!.addEventListener("click", () => setViewMode("text"));
  document.getElementById("btn-view-split")!.addEventListener("click", () => setViewMode("split"));
  document.getElementById("btn-view-graph")!.addEventListener("click", () => setViewMode("graph"));

  // Restore persisted view mode
  const savedView = localStorage.getItem("psflow-view-mode") as ViewMode | null;
  if (savedView && ["text", "split", "graph"].includes(savedView)) {
    setViewMode(savedView);
  }

  // Keyboard shortcuts
  document.addEventListener("keydown", handleKeyboard);

  // Resize handles
  initResizeHandle();
  initGraphResizeHandle();

  document.getElementById("status")!.textContent = "Ready";
}

main().catch(showError);
