/**
 * Graph visualization panel — ELK layout + Sprotty rendering.
 *
 * ELK runs outside Sprotty to compute positions, then the pre-positioned
 * model is passed to Sprotty which handles rendering and interaction only.
 */

import { LocalModelSource, TYPES } from "sprotty";
import type { IActionDispatcher } from "sprotty";
import { SelectAction, CenterAction, FitToScreenAction, UpdateModelAction } from "sprotty-protocol";
import type { SModelIndex } from "sprotty-protocol";
import { ElkLayoutEngine } from "sprotty-elk";
import type { ElkFactory } from "sprotty-elk";
import ElkConstructor from "elkjs/lib/elk.bundled.js";
import type { ParseResult } from "../../pkg/psflow_wasm.js";
import type { NodeState } from "../state.js";
import { buildSprottyModel, NODE } from "./model.js";
import { PsflowLayoutConfigurator } from "./layout.js";
import { createContainer } from "./di-config.js";
import { extractLayoutData, setLayoutData } from "./layout-store.js";

// --- ELK layout engine (runs outside Sprotty) ---

const elkFactory: ElkFactory = () => new ElkConstructor();
const layoutConfigurator = new PsflowLayoutConfigurator();
const layoutEngine = new ElkLayoutEngine(elkFactory, undefined, layoutConfigurator);

// --- Types ---

export interface GraphHandle {
  setGraph(parseResult: ParseResult): void;
  updateNodeStates(states: Map<string, NodeState>): void;
  selectNode(nodeId: string | null): void;
  setShowPorts(show: boolean): void;
  destroy(): void;
}

// --- Public API ---

export function createGraph(
  container: HTMLElement,
  onSelect: (nodeId: string | null) => void,
  onDoubleClick: (nodeId: string) => void,
): GraphHandle {
  let layoutGeneration = 0;
  let showPorts = localStorage.getItem("psflow-show-ports") === "true";
  let lastParseResult: ParseResult | null = null;
  let lastStates: Map<string, NodeState> = new Map();
  let lastPositionedModel: any = null;


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

  function updatePortButton() {
    const btn = zoomBar.querySelector('[data-action="ports"]') as HTMLElement | null;
    if (btn) btn.classList.toggle("active", showPorts);
  }
  updatePortButton();

  // Sprotty container div
  const sprottyDiv = document.createElement("div");
  sprottyDiv.id = `sprotty-${Date.now()}`;
  sprottyDiv.className = "graph-svg-container";
  container.appendChild(sprottyDiv);

  // Initialize Sprotty (rendering only — no layout engine)
  const diContainer = createContainer(sprottyDiv.id, { onSelect, onDoubleClick });
  const modelSource = diContainer.get<LocalModelSource>(TYPES.ModelSource);
  const actionDispatcher = diContainer.get<IActionDispatcher>(TYPES.IActionDispatcher);

  // Zoom bar event handling
  zoomBar.addEventListener("click", (e) => {
    const btn = (e.target as Element).closest("[data-action]") as HTMLElement | null;
    if (!btn) return;
    const action = btn.dataset.action;
    if (action === "zoom-in") {
      actionDispatcher.dispatch(FitToScreenAction.create([], { maxZoom: 999, animate: true }));
    } else if (action === "zoom-out") {
      actionDispatcher.dispatch(FitToScreenAction.create([], { maxZoom: 0.5, animate: true }));
    } else if (action === "fit") {
      actionDispatcher.dispatch(FitToScreenAction.create([], { animate: true }));
    } else if (action === "ports") {
      handle.setShowPorts(!showPorts);
    }
  });

  const handle: GraphHandle = {
    async setGraph(parseResult: ParseResult) {
      lastParseResult = parseResult;
      const gen = ++layoutGeneration;

      if (parseResult.nodes.length === 0) {
        sprottyDiv.innerHTML = '<p class="placeholder">No graph to display</p>';
        return;
      }

      try {
        // Build Sprotty model, run ELK layout externally, then pass to Sprotty
        const model = buildSprottyModel(parseResult, showPorts, lastStates);
        const positioned = await layoutEngine.layout(model, undefined as unknown as SModelIndex);
        lastPositionedModel = positioned;

        // Store layout data externally — views read from this store
        // because Sprotty re-creates model instances during render
        setLayoutData(extractLayoutData(positioned));

        if (gen !== layoutGeneration) return;
        await modelSource.setModel(positioned);
        await actionDispatcher.dispatch(
          FitToScreenAction.create([], { padding: 20, animate: false }),
        );
      } catch (err) {
        if (gen !== layoutGeneration) return;
        console.error("Graph layout failed:", err);
        sprottyDiv.innerHTML = '<p class="placeholder">Layout failed</p>';
      }
    },

    updateNodeStates(states: Map<string, NodeState>) {
      lastStates = states;
      if (!lastPositionedModel) return;

      // Patch cssClasses on the cached positioned model (preserves all ELK data)
      function patchStates(children: any[]) {
        for (const c of children || []) {
          if (c.type === NODE) {
            const state = states.get(c.id) || "idle";
            c.cssClasses = [`node-${state}`];
          }
          if (c.children) patchStates(c.children);
        }
      }
      patchStates(lastPositionedModel.children || []);
      actionDispatcher.dispatch(UpdateModelAction.create(lastPositionedModel, { animate: false }));
    },

    selectNode(nodeId: string | null) {
      if (nodeId) {
        actionDispatcher.dispatch(
          SelectAction.create({ selectedElementsIDs: [nodeId], deselectedElementsIDs: [] }),
        );
        actionDispatcher.dispatch(
          CenterAction.create([nodeId], { animate: true, retainZoom: true }),
        );
      } else {
        actionDispatcher.dispatch(
          SelectAction.create({ selectedElementsIDs: [], deselectedElementsIDs: [] }),
        );
      }
    },

    setShowPorts(show: boolean) {
      if (show === showPorts) return;
      showPorts = show;
      localStorage.setItem("psflow-show-ports", String(show));
      updatePortButton();
      if (lastParseResult) handle.setGraph(lastParseResult);
    },

    destroy() {
      container.innerHTML = "";
    },
  };

  return handle;
}
