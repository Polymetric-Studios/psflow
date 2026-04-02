/**
 * Sprotty event listeners — double-click, selection, keyboard.
 */

import { injectable, inject } from "inversify";
import { MouseListener, KeyListener, SNodeImpl, findParentByFeature, isSelectable } from "sprotty";
import type { SModelElementImpl } from "sprotty";
import type { Action } from "sprotty-protocol";
import { SelectAction, CenterAction } from "sprotty-protocol";
import { NODE } from "./model.js";

// --- Callback Service ---

export const PSFLOW_CALLBACKS = Symbol("PsflowCallbacks");

export interface PsflowCallbacks {
  onSelect: (nodeId: string | null) => void;
  onDoubleClick: (nodeId: string) => void;
}

// --- Double-Click Listener ---

@injectable()
export class PsflowMouseListener extends MouseListener {
  @inject(PSFLOW_CALLBACKS) protected callbacks!: PsflowCallbacks;

  override doubleClick(target: SModelElementImpl, _event: MouseEvent): (Action | Promise<Action>)[] {
    const node = findParentByFeature(target, isSelectable);
    if (node instanceof SNodeImpl && node.type === NODE) {
      this.callbacks.onDoubleClick(node.id);
    }
    return [];
  }
}

// --- Selection Listener ---

@injectable()
export class PsflowSelectionListener extends MouseListener {
  @inject(PSFLOW_CALLBACKS) protected callbacks!: PsflowCallbacks;

  override mouseUp(target: SModelElementImpl, _event: MouseEvent): (Action | Promise<Action>)[] {
    // After Sprotty processes the click, fire our callback
    const node = findParentByFeature(target, isSelectable);
    if (node instanceof SNodeImpl && node.type === NODE) {
      // Defer to let Sprotty process selection first
      setTimeout(() => this.callbacks.onSelect(node.id), 0);
    } else if (target.root === target) {
      // Clicked background
      setTimeout(() => this.callbacks.onSelect(null), 0);
    }
    return [];
  }
}

// --- Keyboard Listener ---

@injectable()
export class PsflowKeyListener extends KeyListener {
  override keyDown(_element: SModelElementImpl, event: KeyboardEvent): Action[] {
    // Arrow key navigation handled natively by Sprotty's focus/tab system
    // Enter/Space to center on focused element
    if (event.key === "Enter" || event.key === " ") {
      const focused = document.activeElement;
      const nodeId = focused?.getAttribute("data-node-id");
      if (nodeId) {
        event.preventDefault();
        return [
          SelectAction.create({ selectedElementsIDs: [nodeId], deselectedElementsIDs: [] }),
          CenterAction.create([nodeId], { animate: true, retainZoom: true }),
        ];
      }
    }
    return [];
  }
}
