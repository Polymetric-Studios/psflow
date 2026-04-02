/**
 * Shared store for ELK layout data.
 *
 * Views read positions and routes from here instead of from Sprotty's
 * model instances, since Sprotty re-creates instances during its render
 * cycle and loses ELK-computed data.
 */

import type { Point } from "sprotty-protocol";

export interface LayoutData {
  positions: Map<string, Point>;
  sizes: Map<string, { width: number; height: number }>;
  routes: Map<string, Point[]>;
}

let current: LayoutData = {
  positions: new Map(),
  sizes: new Map(),
  routes: new Map(),
};

export function setLayoutData(data: LayoutData): void {
  current = data;
}

export function getLayoutData(): LayoutData {
  return current;
}

/** Extract layout data from a positioned Sprotty model (after ELK layout). */
export function extractLayoutData(model: any): LayoutData {
  const positions = new Map<string, Point>();
  const sizes = new Map<string, { width: number; height: number }>();
  const routes = new Map<string, Point[]>();

  function walk(children: any[]) {
    for (const c of children || []) {
      if (c.position) positions.set(c.id, c.position);
      if (c.size) sizes.set(c.id, c.size);
      if (c.routingPoints?.length) routes.set(c.id, c.routingPoints);
      if (c.children) walk(c.children);
    }
  }

  walk(model.children || []);
  return { positions, sizes, routes };
}
