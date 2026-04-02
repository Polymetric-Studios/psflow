/**
 * Sprotty model types and ParseResult → SGraph builder.
 */

import type { SGraph, SNode, SEdge, SPort, SLabel, SModelElement } from "sprotty-protocol";
import type { ParseResult } from "../../pkg/psflow_wasm.js";
import type { NodeState } from "../state.js";

// --- Type Constants ---

export const GRAPH = "graph";
export const NODE = "node:psflow";
export const SUBGRAPH = "node:subgraph";
export const EDGE = "edge:psflow";
export const PORT = "port:psflow";
export const LABEL_NODE = "label:node";
export const LABEL_EDGE = "label:edge";
export const LABEL_PORT = "label:port";
export const LABEL_SUBGRAPH = "label:subgraph";

// --- Text Measurement ---

const FONT_FAMILY = "JetBrains Mono, Fira Code, Cascadia Code, monospace";
const NODE_MIN_WIDTH = 160;
const NODE_MIN_HEIGHT = 40;

let _measureCtx: CanvasRenderingContext2D | null = null;
function getMeasureCtx(): CanvasRenderingContext2D {
  if (!_measureCtx) {
    const canvas = document.createElement("canvas");
    _measureCtx = canvas.getContext("2d")!;
  }
  return _measureCtx;
}

export function measureLabel(text: string, fontSize: number, fontWeight: string | number = "normal"): { width: number; height: number } {
  const ctx = getMeasureCtx();
  ctx.font = `${fontWeight} ${fontSize}px ${FONT_FAMILY}`;
  const metrics = ctx.measureText(text);
  return { width: Math.ceil(metrics.width) + 6, height: fontSize };
}

// --- Port Extraction ---

interface PortInfo {
  name: string;
  type: string;
  direction: "input" | "output";
}

function extractPorts(annotations: { key: string; value: string }[]): PortInfo[] {
  const ports: PortInfo[] = [];
  for (const ann of annotations) {
    if (ann.key.startsWith("inputs.")) {
      ports.push({ name: ann.key.slice(7), type: ann.value.replace(/"/g, ""), direction: "input" });
    } else if (ann.key.startsWith("outputs.")) {
      ports.push({ name: ann.key.slice(8), type: ann.value.replace(/"/g, ""), direction: "output" });
    }
  }
  return ports;
}

// --- Model Builder ---

export function buildSprottyModel(
  parseResult: ParseResult,
  showPorts: boolean,
  nodeStates?: Map<string, NodeState>,
): SGraph {
  const children: SModelElement[] = [];

  // Map nodes to subgraphs
  const nodeToSubgraph = new Map<string, string>();
  for (const sg of parseResult.subgraphs) {
    for (const nid of sg.node_ids) {
      nodeToSubgraph.set(nid, sg.id);
    }
  }

  // Prepare subgraph child buckets
  const subgraphChildren = new Map<string, SModelElement[]>();
  for (const sg of parseResult.subgraphs) {
    subgraphChildren.set(sg.id, []);
  }

  // Extract port info per node (needed for edge routing)
  const nodePorts = new Map<string, PortInfo[]>();

  // Create nodes
  for (const node of parseResult.nodes) {
    const ports = extractPorts(node.annotations);
    nodePorts.set(node.id, ports);

    const nodeChildren: SModelElement[] = [];

    // Node label
    const labelSize = measureLabel(node.label, 13, 500);
    nodeChildren.push({
      type: LABEL_NODE,
      id: `${node.id}_label`,
      text: node.label,
      size: labelSize,
    } as SLabel);

    // Ports
    if (showPorts) {
      const inputs = ports.filter(p => p.direction === "input");
      const outputs = ports.filter(p => p.direction === "output");

      for (const port of inputs) {
        const portId = `${node.id}.in.${port.name}`;
        const portLabelSize = measureLabel(port.name, 9);
        nodeChildren.push({
          type: PORT,
          id: portId,
          size: { width: 6, height: 6 },
          children: [{
            type: LABEL_PORT,
            id: `${portId}_label`,
            text: port.name,
            size: portLabelSize,
          } as SLabel],
        } as SPort & { children: SModelElement[] });
      }

      for (const port of outputs) {
        const portId = `${node.id}.out.${port.name}`;
        const portLabelSize = measureLabel(port.name, 9);
        nodeChildren.push({
          type: PORT,
          id: portId,
          size: { width: 6, height: 6 },
          children: [{
            type: LABEL_PORT,
            id: `${portId}_label`,
            text: port.name,
            size: portLabelSize,
          } as SLabel],
        } as SPort & { children: SModelElement[] });
      }
    }

    const state = nodeStates?.get(node.id) || "idle";
    const snode: SNode & { children: SModelElement[] } = {
      type: NODE,
      id: node.id,
      size: { width: NODE_MIN_WIDTH, height: NODE_MIN_HEIGHT },
      children: nodeChildren,
      cssClasses: [`node-${state}`],
    };

    const sgId = nodeToSubgraph.get(node.id);
    if (sgId && subgraphChildren.has(sgId)) {
      subgraphChildren.get(sgId)!.push(snode);
    } else {
      children.push(snode);
    }
  }

  // Create subgraphs as compound nodes
  for (const sg of parseResult.subgraphs) {
    const sgChildren = subgraphChildren.get(sg.id) || [];

    if (sg.label) {
      const sgLabelSize = measureLabel(sg.label, 11, 600);
      sgChildren.unshift({
        type: LABEL_SUBGRAPH,
        id: `${sg.id}_label`,
        text: sg.label,
        size: sgLabelSize,
      } as SLabel);
    }

    children.push({
      type: SUBGRAPH,
      id: sg.id,
      children: sgChildren,
      cssClasses: ["graph-subgraph"],
    } as SNode & { children: SModelElement[] });
  }

  // Create edges — place inside subgraph when both endpoints share one
  for (let i = 0; i < parseResult.edges.length; i++) {
    const e = parseResult.edges[i];
    let sourceId = e.source;
    let targetId = e.target;

    if (showPorts) {
      const srcPorts = nodePorts.get(e.source) || [];
      const tgtPorts = nodePorts.get(e.target) || [];
      const srcOutputs = srcPorts.filter(p => p.direction === "output");
      const tgtInputs = tgtPorts.filter(p => p.direction === "input");

      for (const out of srcOutputs) {
        const match = tgtInputs.find(inp => inp.name === out.name);
        if (match) {
          sourceId = `${e.source}.out.${out.name}`;
          targetId = `${e.target}.in.${match.name}`;
          break;
        }
      }
    }

    const edgeChildren: SModelElement[] = [];
    if (e.label) {
      const labelSize = measureLabel(e.label, 11, 500);
      edgeChildren.push({
        type: LABEL_EDGE,
        id: `e${i}_label`,
        text: e.label,
        size: labelSize,
      } as SLabel);
    }

    const edge = {
      type: EDGE,
      id: `e${i}`,
      sourceId,
      targetId,
      children: edgeChildren,
    } as SEdge & { children: SModelElement[] };

    // Place edge inside its subgraph if both endpoints are in the same one
    const srcSg = nodeToSubgraph.get(e.source);
    const tgtSg = nodeToSubgraph.get(e.target);
    if (srcSg && srcSg === tgtSg && subgraphChildren.has(srcSg)) {
      subgraphChildren.get(srcSg)!.push(edge);
    } else {
      children.push(edge);
    }
  }

  return {
    type: GRAPH,
    id: "root",
    children,
  } as SGraph;
}
