import { EditorState, StateField, StateEffect } from "@codemirror/state";
import { EditorView, Decoration, DecorationSet, keymap, gutter, GutterMarker } from "@codemirror/view";
import { defaultKeymap } from "@codemirror/commands";
import type { ParseResult } from "../pkg/psflow_wasm.js";
import type { NodeState } from "./state.js";

// --- State effects ---

/** Effect to update node decorations based on execution state. */
export const setNodeStates = StateEffect.define<Map<string, NodeState>>();

/** Effect to set the selected node. */
export const setSelectedNode = StateEffect.define<string | null>();

/** Effect to set the parse result (node ranges). */
export const setParseResult = StateEffect.define<ParseResult>();

// --- Decoration classes ---

const nodeDecorations: Record<string, Decoration> = {
  pending: Decoration.line({ class: "cm-node-pending" }),
  running: Decoration.line({ class: "cm-node-running" }),
  completed: Decoration.line({ class: "cm-node-completed" }),
  failed: Decoration.line({ class: "cm-node-failed" }),
  cancelled: Decoration.line({ class: "cm-node-cancelled" }),
};

const selectedDecoration = Decoration.line({ class: "cm-node-selected" });

const STATE_PRIORITY: Record<string, number> = {
  idle: 0,
  pending: 1,
  cancelled: 2,
  completed: 3,
  failed: 4,
  running: 5,
};

// --- Stored parse result field ---

const parseResultField = StateField.define<ParseResult | null>({
  create: () => null,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setParseResult)) return e.value;
    }
    return value;
  },
});

// --- Node state field ---

const nodeStatesField = StateField.define<Map<string, NodeState>>({
  create: () => new Map(),
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setNodeStates)) return e.value;
    }
    return value;
  },
});

// --- Selected node field ---

const selectedNodeField = StateField.define<string | null>({
  create: () => null,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setSelectedNode)) return e.value;
    }
    return value;
  },
});

// --- Decoration state field ---

const decorationField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(_, tr) {
    const parseResult = tr.state.field(parseResultField);
    const nodeStates = tr.state.field(nodeStatesField);
    const selectedNodeId = tr.state.field(selectedNodeField);
    // Rebuild on every transaction (cheap — just iterating nodes)
    // We need the view for doc access, but StateField.update doesn't have it.
    // Instead, build from state.doc directly.
    return buildDecorationsFromState(tr.state, parseResult, nodeStates, selectedNodeId);
  },
  provide: (f) => EditorView.decorations.from(f),
});

function buildDecorationsFromState(
  state: EditorState,
  parseResult: ParseResult | null,
  nodeStates: Map<string, NodeState>,
  selectedNodeId: string | null
): DecorationSet {
  if (!parseResult) return Decoration.none;

  // Use a Map keyed by line start position to deduplicate.
  // When multiple nodes share a line (chained edges), the highest-priority
  // state wins. Selected decoration is tracked separately.
  const stateByLine = new Map<number, NodeState>();
  const selectedLines = new Set<number>();
  const doc = state.doc;

  for (const node of parseResult.nodes) {
    const nState = nodeStates.get(node.id);
    const isSelected = node.id === selectedNodeId;

    const lineSet = new Set<number>();

    if (node.definition.from < doc.length) {
      lineSet.add(doc.lineAt(node.definition.from).number);
    }
    for (const ann of node.annotations) {
      if (ann.span.from < doc.length) {
        lineSet.add(doc.lineAt(ann.span.from).number);
      }
    }

    for (const lineNum of lineSet) {
      const line = doc.line(lineNum);
      if (nState && nState !== "idle" && nodeDecorations[nState]) {
        const existing = stateByLine.get(line.from);
        // Higher-priority state wins (running > completed > pending > etc.)
        if (!existing || STATE_PRIORITY[nState] > (STATE_PRIORITY[existing] ?? 0)) {
          stateByLine.set(line.from, nState);
        }
      }
      if (isSelected) {
        selectedLines.add(line.from);
      }
    }
  }

  // Build deduplicated decorations sorted by position
  const positions = new Set([...stateByLine.keys(), ...selectedLines]);
  const sorted = [...positions].sort((a, b) => a - b);
  const decorations: ReturnType<Decoration["range"]>[] = [];

  for (const from of sorted) {
    const nState = stateByLine.get(from);
    if (nState && nodeDecorations[nState]) {
      decorations.push(nodeDecorations[nState].range(from));
    }
    if (selectedLines.has(from)) {
      decorations.push(selectedDecoration.range(from));
    }
  }

  return Decoration.set(decorations, true);
}

// --- Gutter ---

class StateDotMarker extends GutterMarker {
  constructor(readonly state: NodeState) {
    super();
  }
  toDOM(): Node {
    const dot = document.createElement("span");
    dot.className = `cm-state-dot ${this.state}`;
    return dot;
  }
}

const stateGutter = gutter({
  class: "cm-gutter-state",
  lineMarker(view, line) {
    const parseResult = view.state.field(parseResultField);
    const nodeStates = view.state.field(nodeStatesField);
    if (!parseResult) return null;

    // Find if this line belongs to a node definition
    for (const node of parseResult.nodes) {
      if (node.definition.from < view.state.doc.length) {
        const defLine = view.state.doc.lineAt(node.definition.from);
        if (defLine.from === line.from) {
          const state = nodeStates.get(node.id);
          if (state && state !== "idle") {
            return new StateDotMarker(state);
          }
        }
      }
    }
    return null;
  },
});

// --- Create editor ---

export interface EditorHandle {
  view: EditorView;
  setSource(source: string): void;
  updateParseResult(result: ParseResult): void;
  updateNodeStates(states: Map<string, NodeState>): void;
  selectNode(nodeId: string | null): void;
  scrollToNode(nodeId: string): void;
}

export function createEditor(
  parent: HTMLElement,
  onNodeSelect: (nodeId: string | null) => void
): EditorHandle {
  const view = new EditorView({
    state: EditorState.create({
      doc: "",
      extensions: [
        EditorView.editable.of(false),
        keymap.of(defaultKeymap),
        parseResultField,
        nodeStatesField,
        selectedNodeField,
        decorationField,
        stateGutter,
        EditorView.domEventHandlers({
          click(event, view) {
            const pos = view.posAtCoords({ x: event.clientX, y: event.clientY });
            if (pos === null) return;

            const parseResult = view.state.field(parseResultField);
            if (!parseResult) return;

            const clickedLine = view.state.doc.lineAt(pos);

            // Find which node this line belongs to
            for (const node of parseResult.nodes) {
              const lines = new Set<number>();
              if (node.definition.from < view.state.doc.length) {
                lines.add(view.state.doc.lineAt(node.definition.from).number);
              }
              for (const ann of node.annotations) {
                if (ann.span.from < view.state.doc.length) {
                  lines.add(view.state.doc.lineAt(ann.span.from).number);
                }
              }

              if (lines.has(clickedLine.number)) {
                onNodeSelect(node.id);
                return;
              }
            }
            // Clicked outside any node — deselect
            onNodeSelect(null);
          },
        }),
        EditorView.theme({
          "&": {
            backgroundColor: "#1e1e2e",
            color: "#cdd6f4",
          },
          ".cm-gutters": {
            backgroundColor: "#24243a",
            borderRight: "1px solid #3a3a5c",
          },
          ".cm-activeLineGutter": {
            backgroundColor: "transparent",
          },
          ".cm-cursor": {
            borderLeftColor: "#89b4fa",
          },
        }),
      ],
    }),
    parent,
  });

  return {
    view,

    setSource(source: string) {
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: source },
      });
    },

    updateParseResult(result: ParseResult) {
      view.dispatch({ effects: setParseResult.of(result) });
    },

    updateNodeStates(states: Map<string, NodeState>) {
      view.dispatch({ effects: setNodeStates.of(states) });
    },

    selectNode(nodeId: string | null) {
      view.dispatch({ effects: setSelectedNode.of(nodeId) });
    },

    scrollToNode(nodeId: string) {
      const parseResult = view.state.field(parseResultField);
      if (!parseResult) return;
      const node = parseResult.nodes.find((n) => n.id === nodeId);
      if (!node || node.definition.from >= view.state.doc.length) return;
      const line = view.state.doc.lineAt(node.definition.from);
      view.dispatch({
        effects: EditorView.scrollIntoView(line.from, { y: "center" }),
      });
    },
  };
}
