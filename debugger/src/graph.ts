/**
 * Graph visualization panel — ELK.js layout + custom SVG renderer.
 *
 * Renders the parsed .mmd graph as an interactive SVG with execution
 * state styling that mirrors the CodeMirror decorations.
 */

import ELK, { type ElkNode, type ElkExtendedEdge } from "elkjs";
import type { ParseResult } from "../pkg/psflow_wasm.js";
import type { NodeState } from "./state.js";

// --- Types ---

interface PortInfo {
  name: string;
  type: string;
  direction: "input" | "output";
}

interface LayoutNode {
  id: string;
  label: string;
  x: number;
  y: number;
  width: number;
  height: number;
  /** Subgraph this node belongs to, if any */
  parent?: string;
  /** Port definitions extracted from annotations */
  ports: PortInfo[];
}

interface LayoutEdge {
  source: string;
  target: string;
  label?: string;
  /** SVG path data string computed from ELK waypoints */
  path: string;
  /** Label position */
  labelX?: number;
  labelY?: number;
}

interface LayoutSubgraph {
  id: string;
  label?: string;
  x: number;
  y: number;
  width: number;
  height: number;
}

interface Layout {
  nodes: LayoutNode[];
  edges: LayoutEdge[];
  subgraphs: LayoutSubgraph[];
  width: number;
  height: number;
}

export interface GraphHandle {
  setGraph(parseResult: ParseResult): void;
  updateNodeStates(states: Map<string, NodeState>): void;
  selectNode(nodeId: string | null): void;
  setShowPorts(show: boolean): void;
  destroy(): void;
}

// --- Constants ---

const NODE_WIDTH = 160;
const NODE_HEIGHT = 40;
const NODE_PADDING_X = 16;
const SUBGRAPH_PADDING = 24;
const LABEL_FONT_SIZE = 11;

// --- ELK Layout ---

const elk = new ELK();

async function computeLayout(parseResult: ParseResult): Promise<Layout> {
  // Determine which nodes belong to subgraphs
  const nodeToSubgraph = new Map<string, string>();
  for (const sg of parseResult.subgraphs) {
    for (const nid of sg.node_ids) {
      nodeToSubgraph.set(nid, sg.id);
    }
  }

  // Build ELK graph with subgraphs as compound nodes
  const topLevelNodes: ElkNode[] = [];
  const subgraphElkNodes = new Map<string, ElkNode>();

  // Create subgraph containers first
  for (const sg of parseResult.subgraphs) {
    const sgNode: ElkNode = {
      id: `sg_${sg.id}`,
      labels: sg.label ? [{ text: sg.label }] : [],
      children: [],
      layoutOptions: {
        "elk.padding": `[top=${SUBGRAPH_PADDING + 16},left=${SUBGRAPH_PADDING},bottom=${SUBGRAPH_PADDING},right=${SUBGRAPH_PADDING}]`,
      },
    };
    subgraphElkNodes.set(sg.id, sgNode);
    topLevelNodes.push(sgNode);
  }

  // Extract port info from annotations and create node elements
  const nodePorts = new Map<string, PortInfo[]>();
  for (const node of parseResult.nodes) {
    const ports: PortInfo[] = [];
    for (const ann of node.annotations) {
      if (ann.key.startsWith("inputs.")) {
        ports.push({ name: ann.key.slice(7), type: ann.value.replace(/"/g, ""), direction: "input" });
      } else if (ann.key.startsWith("outputs.")) {
        ports.push({ name: ann.key.slice(8), type: ann.value.replace(/"/g, ""), direction: "output" });
      }
    }
    nodePorts.set(node.id, ports);

    const estimatedWidth = Math.max(NODE_WIDTH, node.label.length * 8 + NODE_PADDING_X * 2);

    const elkNode: ElkNode = {
      id: node.id,
      width: estimatedWidth,
      height: NODE_HEIGHT,
      labels: [{ text: node.label }],
    };

    const sgId = nodeToSubgraph.get(node.id);
    if (sgId && subgraphElkNodes.has(sgId)) {
      subgraphElkNodes.get(sgId)!.children!.push(elkNode);
    } else {
      topLevelNodes.push(elkNode);
    }
  }

  // Place edges at lowest common ancestor (intra-subgraph edges go on the subgraph)
  const rootEdges: ElkExtendedEdge[] = [];
  for (let i = 0; i < parseResult.edges.length; i++) {
    const e = parseResult.edges[i];
    const elkEdge: ElkExtendedEdge = {
      id: `e${i}`,
      sources: [e.source],
      targets: [e.target],
      labels: e.label ? [{ text: e.label, width: e.label.length * 7, height: 14 }] : [],
    };

    const srcSg = nodeToSubgraph.get(e.source);
    const tgtSg = nodeToSubgraph.get(e.target);
    if (srcSg && srcSg === tgtSg && subgraphElkNodes.has(srcSg)) {
      // Both endpoints in same subgraph — place edge there
      const sgNode = subgraphElkNodes.get(srcSg)!;
      if (!sgNode.edges) sgNode.edges = [];
      sgNode.edges.push(elkEdge);
    } else {
      rootEdges.push(elkEdge);
    }
  }

  const graph: ElkNode = {
    id: "root",
    children: topLevelNodes,
    edges: rootEdges,
    layoutOptions: {
      "elk.algorithm": "layered",
      "elk.direction": "DOWN",
      "elk.spacing.nodeNode": "30",
      "elk.spacing.edgeNode": "20",
      "elk.layered.spacing.nodeNodeBetweenLayers": "50",
      "elk.layered.spacing.edgeEdgeBetweenLayers": "20",
      "elk.edgeRouting": "ORTHOGONAL",
      "elk.layered.mergeEdges": "true",
    },
  };

  const laid = await elk.layout(graph);

  // Extract positioned nodes
  const nodes: LayoutNode[] = [];
  const subgraphs: LayoutSubgraph[] = [];

  function extractNodes(parent: ElkNode, offsetX = 0, offsetY = 0, parentSgId?: string) {
    for (const child of parent.children || []) {
      const cx = (child.x || 0) + offsetX;
      const cy = (child.y || 0) + offsetY;

      // Is this a subgraph container?
      if (child.id.startsWith("sg_")) {
        const sgId = child.id.slice(3);
        subgraphs.push({
          id: sgId,
          label: child.labels?.[0]?.text,
          x: cx,
          y: cy,
          width: child.width || 0,
          height: child.height || 0,
        });
        extractNodes(child, cx, cy, sgId);
      } else {
        nodes.push({
          id: child.id,
          label: child.labels?.[0]?.text || child.id,
          x: cx,
          y: cy,
          width: child.width || NODE_WIDTH,
          height: child.height || NODE_HEIGHT,
          parent: parentSgId,
          ports: nodePorts.get(child.id) || [],
        });
      }
    }
  }

  extractNodes(laid);

  // Extract positioned edges (from root and subgraphs)
  const edges: LayoutEdge[] = [];

  function extractEdges(parent: ElkNode, offsetX = 0, offsetY = 0) {
    for (const edge of parent.edges || []) {
      const e = edge as ElkExtendedEdge & { sections?: any[] };
      if (!e.sections?.length) continue;

      const section = e.sections[0];
      const points = [
        section.startPoint,
        ...(section.bendPoints || []),
        section.endPoint,
      ].map(p => ({ x: p.x + offsetX, y: p.y + offsetY }));

      let path = `M ${points[0].x} ${points[0].y}`;
      for (let i = 1; i < points.length; i++) {
        path += ` L ${points[i].x} ${points[i].y}`;
      }

      const label = e.labels?.[0];

      edges.push({
        source: e.sources[0],
        target: e.targets[0],
        label: label?.text,
        path,
        labelX: label ? (label.x || 0) + offsetX : undefined,
        labelY: label ? (label.y || 0) + offsetY : undefined,
      });
    }

    // Recurse into subgraph children
    for (const child of parent.children || []) {
      if (child.id.startsWith("sg_")) {
        extractEdges(child, offsetX + (child.x || 0), offsetY + (child.y || 0));
      }
    }
  }

  extractEdges(laid);

  return {
    nodes,
    edges,
    subgraphs,
    width: laid.width || 400,
    height: laid.height || 300,
  };
}

// --- SVG Renderer ---

const SVG_NS = "http://www.w3.org/2000/svg";

function createSvgElement<K extends keyof SVGElementTagNameMap>(
  tag: K,
  attrs: Record<string, string | number> = {},
): SVGElementTagNameMap[K] {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs)) {
    el.setAttribute(k, String(v));
  }
  return el;
}

function renderGraph(
  container: HTMLElement,
  layout: Layout,
  onSelect: (nodeId: string | null) => void,
  onDoubleClick: (nodeId: string) => void,
  showPorts: boolean,
): { svg: SVGSVGElement; nodeEls: Map<string, SVGGElement> } {
  // Clear previous
  container.innerHTML = "";

  const padding = 32;
  const totalWidth = layout.width + padding * 2;
  const totalHeight = layout.height + padding * 2;

  const svg = createSvgElement("svg", {
    width: "100%",
    height: "100%",
    viewBox: `0 0 ${totalWidth} ${totalHeight}`,
    class: "graph-svg",
  });
  svg.style.display = "block";

  // Arrowhead marker
  const defs = createSvgElement("defs");
  const marker = createSvgElement("marker", {
    id: "arrowhead",
    viewBox: "0 0 10 7",
    refX: "10",
    refY: "3.5",
    markerWidth: "8",
    markerHeight: "6",
    orient: "auto-start-reverse",
  });
  const arrowPath = createSvgElement("path", {
    d: "M 0 0 L 10 3.5 L 0 7 z",
    class: "graph-arrowhead",
  });
  marker.appendChild(arrowPath);
  defs.appendChild(marker);
  svg.appendChild(defs);

  // Content group for zoom/pan
  const contentGroup = createSvgElement("g", {
    class: "graph-content",
    transform: `translate(${padding}, ${padding})`,
  });
  svg.appendChild(contentGroup);

  // Render subgraphs (background)
  for (const sg of layout.subgraphs) {
    const g = createSvgElement("g", { class: "graph-subgraph" });
    const rect = createSvgElement("rect", {
      x: sg.x,
      y: sg.y,
      width: sg.width,
      height: sg.height,
      rx: 8,
      class: "graph-subgraph-rect",
    });
    g.appendChild(rect);

    if (sg.label) {
      const text = createSvgElement("text", {
        x: sg.x + 10,
        y: sg.y + 16,
        class: "graph-subgraph-label",
      });
      text.textContent = sg.label;
      g.appendChild(text);
    }
    contentGroup.appendChild(g);
  }

  // Render edges
  for (const edge of layout.edges) {
    const g = createSvgElement("g", { class: "graph-edge" });
    const path = createSvgElement("path", {
      d: edge.path,
      class: "graph-edge-path",
      "marker-end": "url(#arrowhead)",
    });
    g.appendChild(path);

    if (edge.label && edge.labelX != null && edge.labelY != null) {
      const text = createSvgElement("text", {
        x: edge.labelX,
        y: edge.labelY + LABEL_FONT_SIZE,
        class: "graph-edge-label",
      });
      text.textContent = edge.label;
      g.appendChild(text);
    }
    contentGroup.appendChild(g);
  }

  // Render nodes
  const nodeEls = new Map<string, SVGGElement>();
  const nodeOrder: string[] = [];
  for (const node of layout.nodes) {
    nodeOrder.push(node.id);
    const g = createSvgElement("g", {
      class: "graph-node node-idle",
      "data-node-id": node.id,
      transform: `translate(${node.x}, ${node.y})`,
      tabindex: "0",
      role: "button",
      "aria-label": `Node: ${node.label}`,
    });

    const rect = createSvgElement("rect", {
      width: node.width,
      height: node.height,
      rx: 6,
      class: "graph-node-rect",
    });
    g.appendChild(rect);

    const text = createSvgElement("text", {
      x: node.width / 2,
      y: node.height / 2,
      "dominant-baseline": "central",
      "text-anchor": "middle",
      class: "graph-node-label",
    });
    text.textContent = node.label;
    g.appendChild(text);

    // Port labels (when enabled)
    if (showPorts && node.ports.length > 0) {
      const inputs = node.ports.filter(p => p.direction === "input");
      const outputs = node.ports.filter(p => p.direction === "output");

      // Input ports on left
      for (let i = 0; i < inputs.length; i++) {
        const py = (node.height / (inputs.length + 1)) * (i + 1);
        // Port dot
        g.appendChild(createSvgElement("circle", {
          cx: 0, cy: py, r: 3, class: "graph-port-dot input",
        }));
        // Port label
        const pt = createSvgElement("text", {
          x: -6, y: py,
          "dominant-baseline": "central",
          "text-anchor": "end",
          class: "graph-port-label",
        });
        pt.textContent = inputs[i].name;
        g.appendChild(pt);
      }

      // Output ports on right
      for (let i = 0; i < outputs.length; i++) {
        const py = (node.height / (outputs.length + 1)) * (i + 1);
        g.appendChild(createSvgElement("circle", {
          cx: node.width, cy: py, r: 3, class: "graph-port-dot output",
        }));
        const pt = createSvgElement("text", {
          x: node.width + 6, y: py,
          "dominant-baseline": "central",
          "text-anchor": "start",
          class: "graph-port-label",
        });
        pt.textContent = outputs[i].name;
        g.appendChild(pt);
      }
    }

    g.addEventListener("click", (e) => {
      e.stopPropagation();
      onSelect(node.id);
    });
    g.addEventListener("dblclick", (e) => {
      e.stopPropagation();
      onDoubleClick(node.id);
    });
    // Keyboard: Enter/Space to select (stopPropagation prevents global playback shortcuts)
    g.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        e.stopPropagation();
        onSelect(node.id);
      }
    });

    contentGroup.appendChild(g);
    nodeEls.set(node.id, g);
  }

  // Keyboard navigation between nodes (arrow keys on focused node)
  svg.addEventListener("keydown", (e) => {
    const focused = document.activeElement as Element | null;
    if (!focused?.classList.contains("graph-node")) return;
    const currentId = focused.getAttribute("data-node-id");
    if (!currentId) return;

    let idx = nodeOrder.indexOf(currentId);
    if (idx < 0) return;

    if (e.key === "ArrowDown" || e.key === "ArrowRight") {
      idx = Math.min(idx + 1, nodeOrder.length - 1);
    } else if (e.key === "ArrowUp" || e.key === "ArrowLeft") {
      idx = Math.max(idx - 1, 0);
    } else {
      return;
    }
    e.preventDefault();
    e.stopPropagation();
    const nextEl = nodeEls.get(nodeOrder[idx]);
    if (nextEl) {
      (nextEl as unknown as HTMLElement).focus();
      onSelect(nodeOrder[idx]);
    }
  });

  // Click background to deselect
  svg.addEventListener("click", () => onSelect(null));

  container.appendChild(svg);

  return { svg, nodeEls };
}

// --- Zoom & Pan ---

interface ZoomPanControls {
  resetZoom: () => void;
  zoomIn: () => void;
  zoomOut: () => void;
  fitToView: () => void;
  panToNode: (x: number, y: number, w: number, h: number) => void;
  destroy: () => void;
}

function initZoomPan(svg: SVGSVGElement): ZoomPanControls {
  const viewBox = svg.viewBox.baseVal;
  const origVB = { x: viewBox.x, y: viewBox.y, w: viewBox.width, h: viewBox.height };

  let currentVB = { ...origVB };
  let isPanning = false;
  let panStart = { x: 0, y: 0 };

  function applyViewBox() {
    svg.setAttribute("viewBox", `${currentVB.x} ${currentVB.y} ${currentVB.w} ${currentVB.h}`);
  }

  // Wheel zoom
  function onWheel(e: WheelEvent) {
    e.preventDefault();
    const factor = e.deltaY > 0 ? 1.1 : 0.9;

    const rect = svg.getBoundingClientRect();
    const mx = (e.clientX - rect.left) / rect.width;
    const my = (e.clientY - rect.top) / rect.height;

    const newW = currentVB.w * factor;
    const newH = currentVB.h * factor;

    if (newW > origVB.w * 4 || newW < origVB.w * 0.2) return;

    currentVB.x += (currentVB.w - newW) * mx;
    currentVB.y += (currentVB.h - newH) * my;
    currentVB.w = newW;
    currentVB.h = newH;
    applyViewBox();
  }

  function onMouseDown(e: MouseEvent) {
    if (e.button !== 0) return;
    const target = e.target as Element;
    if (target.closest(".graph-node")) return;

    isPanning = true;
    panStart = { x: e.clientX, y: e.clientY };
    svg.style.cursor = "grabbing";
    e.preventDefault();
  }

  function onMouseMove(e: MouseEvent) {
    if (!isPanning) return;
    const rect = svg.getBoundingClientRect();
    const scaleX = currentVB.w / rect.width;
    const scaleY = currentVB.h / rect.height;

    currentVB.x -= (e.clientX - panStart.x) * scaleX;
    currentVB.y -= (e.clientY - panStart.y) * scaleY;
    panStart = { x: e.clientX, y: e.clientY };
    applyViewBox();
  }

  function onMouseUp() {
    if (isPanning) {
      isPanning = false;
      svg.style.cursor = "";
    }
  }

  svg.addEventListener("wheel", onWheel, { passive: false });
  svg.addEventListener("mousedown", onMouseDown);
  window.addEventListener("mousemove", onMouseMove);
  window.addEventListener("mouseup", onMouseUp);

  function resetZoom() {
    currentVB = { ...origVB };
    applyViewBox();
  }

  function zoomIn() {
    const factor = 0.8;
    const cx = currentVB.x + currentVB.w / 2;
    const cy = currentVB.y + currentVB.h / 2;
    currentVB.w *= factor;
    currentVB.h *= factor;
    currentVB.x = cx - currentVB.w / 2;
    currentVB.y = cy - currentVB.h / 2;
    applyViewBox();
  }

  function zoomOut() {
    const factor = 1.25;
    const newW = currentVB.w * factor;
    if (newW > origVB.w * 4) return;
    const cx = currentVB.x + currentVB.w / 2;
    const cy = currentVB.y + currentVB.h / 2;
    currentVB.w *= factor;
    currentVB.h *= factor;
    currentVB.x = cx - currentVB.w / 2;
    currentVB.y = cy - currentVB.h / 2;
    applyViewBox();
  }

  function fitToView() {
    resetZoom();
  }

  function panToNode(x: number, y: number, w: number, h: number) {
    // Center the viewBox on the node
    const nodeCx = x + w / 2;
    const nodeCy = y + h / 2;
    currentVB.x = nodeCx - currentVB.w / 2;
    currentVB.y = nodeCy - currentVB.h / 2;
    applyViewBox();
  }

  function destroy() {
    svg.removeEventListener("wheel", onWheel);
    svg.removeEventListener("mousedown", onMouseDown);
    window.removeEventListener("mousemove", onMouseMove);
    window.removeEventListener("mouseup", onMouseUp);
  }

  return { resetZoom, zoomIn, zoomOut, fitToView, panToNode, destroy };
}

// --- Public API ---

export function createGraph(
  container: HTMLElement,
  onSelect: (nodeId: string | null) => void,
  onDoubleClick: (nodeId: string) => void,
): GraphHandle {
  let nodeEls = new Map<string, SVGGElement>();
  let layoutNodes = new Map<string, LayoutNode>();
  let svg: SVGSVGElement | null = null;
  let zoomControls: ZoomPanControls | null = null;
  let currentSelected: string | null = null;
  let layoutGeneration = 0;
  let showPorts = localStorage.getItem("psflow-show-ports") === "true";
  let lastParseResult: ParseResult | null = null;

  // Zoom control bar
  const zoomBar = document.createElement("div");
  zoomBar.className = "graph-zoom-controls";
  zoomBar.innerHTML = `
    <button class="btn-transport" title="Zoom in (+)" data-action="zoom-in">+</button>
    <button class="btn-transport" title="Zoom out (-)" data-action="zoom-out">&minus;</button>
    <button class="btn-transport" title="Fit to view (0)" data-action="fit">Fit</button>
    <span class="graph-zoom-divider"></span>
    <button class="btn-transport" title="Toggle port names" data-action="ports">Ports</button>
  `;
  container.appendChild(zoomBar);

  // Update port toggle button state
  function updatePortButton() {
    const btn = zoomBar.querySelector('[data-action="ports"]') as HTMLElement | null;
    if (btn) btn.classList.toggle("active", showPorts);
  }
  updatePortButton();

  zoomBar.addEventListener("click", (e) => {
    const btn = (e.target as Element).closest("[data-action]") as HTMLElement | null;
    if (!btn) return;
    const action = btn.dataset.action;
    if (action === "zoom-in" && zoomControls) zoomControls.zoomIn();
    else if (action === "zoom-out" && zoomControls) zoomControls.zoomOut();
    else if (action === "fit" && zoomControls) zoomControls.fitToView();
    else if (action === "ports") {
      handle.setShowPorts(!showPorts);
    }
  });

  // SVG container
  const svgContainer = document.createElement("div");
  svgContainer.className = "graph-svg-container";
  container.appendChild(svgContainer);

  function cleanup() {
    zoomControls?.destroy();
    nodeEls.clear();
    layoutNodes.clear();
    svg = null;
    zoomControls = null;
  }

  const handle: GraphHandle = {
    async setGraph(parseResult: ParseResult) {
      lastParseResult = parseResult;
      const gen = ++layoutGeneration;

      if (parseResult.nodes.length === 0) {
        cleanup();
        svgContainer.innerHTML = '<p class="placeholder">No graph to display</p>';
        return;
      }

      try {
        const layout = await computeLayout(parseResult);

        // Discard result if a newer setGraph call started while we awaited
        if (gen !== layoutGeneration) return;

        cleanup();
        const result = renderGraph(svgContainer, layout, onSelect, onDoubleClick, showPorts);
        svg = result.svg;
        nodeEls = result.nodeEls;
        zoomControls = initZoomPan(svg);

        // Store layout positions for pan-to-node
        for (const n of layout.nodes) {
          layoutNodes.set(n.id, n);
        }
      } catch (err) {
        if (gen !== layoutGeneration) return;
        console.error("Graph layout failed:", err);
        cleanup();
        svgContainer.innerHTML = '<p class="placeholder">Layout failed</p>';
      }
    },

    updateNodeStates(states: Map<string, NodeState>) {
      for (const [nodeId, el] of nodeEls) {
        const state = states.get(nodeId) || "idle";
        const isSelected = el.classList.contains("node-selected");
        el.className.baseVal = `graph-node node-${state}`;
        if (isSelected) el.classList.add("node-selected");
      }
    },

    selectNode(nodeId: string | null) {
      // Remove previous selection
      if (currentSelected) {
        const prev = nodeEls.get(currentSelected);
        if (prev) prev.classList.remove("node-selected");
      }
      // Add new selection
      if (nodeId) {
        const el = nodeEls.get(nodeId);
        if (el) {
          el.classList.add("node-selected");
          // Pan viewBox to center on selected node
          const ln = layoutNodes.get(nodeId);
          if (ln && zoomControls) {
            zoomControls.panToNode(ln.x, ln.y, ln.width, ln.height);
          }
        }
      }
      currentSelected = nodeId;
    },

    setShowPorts(show: boolean) {
      if (show === showPorts) return;
      showPorts = show;
      localStorage.setItem("psflow-show-ports", String(show));
      updatePortButton();
      // Re-render — setGraph's generation counter handles concurrent calls
      if (lastParseResult) handle.setGraph(lastParseResult);
    },

    destroy() {
      cleanup();
      container.innerHTML = "";
    },
  };

  return handle;
}
