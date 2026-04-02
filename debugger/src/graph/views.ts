/**
 * Sprotty SVG views — renders each model element type.
 *
 * Positions, sizes, and edge routes are read from the layout store
 * (populated by ELK before Sprotty renders) rather than from model
 * instances, which Sprotty re-creates during its render cycle.
 */

import { injectable } from "inversify";
import type { VNode } from "snabbdom";
import { h } from "snabbdom";
import { SGraphView } from "sprotty";
import type { IView, RenderingContext } from "sprotty";
import type { SGraphImpl, SNodeImpl, SEdgeImpl, SPortImpl, SLabelImpl } from "sprotty";
import { LABEL_NODE, LABEL_EDGE, LABEL_PORT, LABEL_SUBGRAPH } from "./model.js";
import { getLayoutData } from "./layout-store.js";

// --- Graph View ---

@injectable()
export class PsflowGraphView extends SGraphView {
  override render(model: Readonly<SGraphImpl>, context: RenderingContext): VNode {
    const vnode = super.render(model, context);
    const defs = h("defs", [
      h("marker", {
        attrs: {
          id: "arrowhead",
          viewBox: "0 0 10 7",
          refX: "10",
          refY: "3.5",
          markerWidth: "8",
          markerHeight: "6",
          orient: "auto-start-reverse",
        },
      }, [
        h("path", { attrs: { d: "M 0 0 L 10 3.5 L 0 7 z", class: "graph-arrowhead" } }),
      ]),
    ]);
    if (vnode.children) {
      vnode.children.unshift(defs);
    }
    return vnode;
  }
}

// --- Node View ---

@injectable()
export class PsflowNodeView implements IView {
  render(node: Readonly<SNodeImpl>, context: RenderingContext): VNode {
    const layout = getLayoutData();
    const size = layout.sizes.get(node.id) || node.size || { width: 160, height: 40 };
    const pos = layout.positions.get(node.id) || node.position;

    const classes: Record<string, boolean> = { "graph-node": true };
    for (const cls of (node.cssClasses || [])) {
      classes[cls] = true;
    }
    if (node.selected) {
      classes["node-selected"] = true;
    }

    const attrs: Record<string, string> = {
      "data-node-id": node.id,
      tabindex: "0",
      role: "button",
      "aria-label": `Node: ${node.id}`,
    };
    if (pos) {
      attrs.transform = `translate(${pos.x}, ${pos.y})`;
    }

    return h("g", { class: classes, attrs }, [
      h("rect", {
        attrs: { width: size.width, height: size.height, rx: 6 },
        class: { "graph-node-rect": true },
      }),
      ...context.renderChildren(node),
    ]);
  }
}

// --- Subgraph View ---

@injectable()
export class PsflowSubgraphView implements IView {
  render(node: Readonly<SNodeImpl>, context: RenderingContext): VNode {
    const layout = getLayoutData();
    const size = layout.sizes.get(node.id) || node.size || { width: 200, height: 100 };
    const pos = layout.positions.get(node.id) || node.position;

    const attrs: Record<string, string> = {};
    if (pos) {
      attrs.transform = `translate(${pos.x}, ${pos.y})`;
    }

    return h("g", { class: { "graph-subgraph": true }, attrs }, [
      h("rect", {
        attrs: { width: size.width, height: size.height, rx: 8 },
        class: { "graph-subgraph-rect": true },
      }),
      ...context.renderChildren(node),
    ]);
  }
}

// --- Edge View ---

@injectable()
export class PsflowEdgeView implements IView {
  render(edge: Readonly<SEdgeImpl>, context: RenderingContext): VNode {
    const layout = getLayoutData();
    const points = layout.routes.get(edge.id);

    if (!points || points.length === 0) {
      return h("g", { class: { "graph-edge": true } }, context.renderChildren(edge));
    }

    let d = `M ${points[0].x} ${points[0].y}`;
    for (let i = 1; i < points.length; i++) {
      d += ` L ${points[i].x} ${points[i].y}`;
    }

    return h("g", { class: { "graph-edge": true } }, [
      h("path", {
        attrs: { d, "marker-end": "url(#arrowhead)" },
        class: { "graph-edge-path": true },
      }),
      ...context.renderChildren(edge),
    ]);
  }
}

// --- Port View ---

@injectable()
export class PsflowPortView implements IView {
  render(port: Readonly<SPortImpl>, context: RenderingContext): VNode {
    const layout = getLayoutData();
    const pos = layout.positions.get(port.id) || port.position;
    const isInput = port.id.includes(".in.");

    const attrs: Record<string, string> = {};
    if (pos) {
      attrs.transform = `translate(${pos.x}, ${pos.y})`;
    }

    return h("g", { attrs }, [
      h("circle", {
        attrs: { cx: 3, cy: 3, r: 3 },
        class: {
          "graph-port-dot": true,
          "input": isInput,
          "output": !isInput,
        },
      }),
      ...context.renderChildren(port),
    ]);
  }
}

// --- Label View ---

@injectable()
export class PsflowLabelView implements IView {
  render(label: Readonly<SLabelImpl>, _context: RenderingContext): VNode {
    let labelClass = "graph-node-label";
    switch (label.type) {
      case LABEL_EDGE: labelClass = "graph-edge-label"; break;
      case LABEL_PORT: labelClass = "graph-port-label"; break;
      case LABEL_SUBGRAPH: labelClass = "graph-subgraph-label"; break;
      case LABEL_NODE: labelClass = "graph-node-label"; break;
    }

    const layout = getLayoutData();
    const pos = layout.positions.get(label.id) || label.position;
    const ali = label.alignment;
    let transform = "";
    if (pos) {
      transform = `translate(${pos.x}, ${pos.y})`;
    }
    if (ali) {
      transform += ` translate(${ali.x}, ${ali.y})`;
    }

    return h("text", {
      class: { [labelClass]: true },
      attrs: {
        "dominant-baseline": "hanging",
        ...(transform ? { transform: transform.trim() } : {}),
      },
    }, label.text);
  }
}
