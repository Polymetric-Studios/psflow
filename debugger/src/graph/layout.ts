/**
 * ELK layout configuration — maps Sprotty element types to ELK layout options.
 */

import { injectable } from "inversify";
import { DefaultLayoutConfigurator } from "sprotty-elk";
import type { SGraph, SNode, SPort, SLabel, SModelIndex } from "sprotty-protocol";
import type { LayoutOptions } from "elkjs/lib/elk-api";
import { SUBGRAPH } from "./model.js";

@injectable()
export class PsflowLayoutConfigurator extends DefaultLayoutConfigurator {

  protected override graphOptions(_sgraph: SGraph, _index: SModelIndex): LayoutOptions {
    return {
      "elk.algorithm": "layered",
      "elk.direction": "DOWN",
      "elk.hierarchyHandling": "INCLUDE_CHILDREN",
      "elk.spacing.nodeNode": "30",
      "elk.spacing.edgeNode": "20",
      "elk.spacing.edgeEdge": "15",
      "elk.spacing.labelNode": "5",
      "elk.layered.spacing.nodeNodeBetweenLayers": "50",
      "elk.layered.spacing.edgeEdgeBetweenLayers": "20",
      "elk.edgeRouting": "ORTHOGONAL",
      "elk.layered.mergeEdges": "true",
      "elk.layered.unnecessaryBendpoints": "true",
    };
  }

  protected override nodeOptions(snode: SNode, _index: SModelIndex): LayoutOptions {
    if (snode.type === SUBGRAPH) {
      return {
        "elk.padding": "[top=40,left=24,bottom=24,right=24]",
        "nodeLabels.placement": "H_LEFT V_TOP INSIDE",
      };
    }

    const hasPorts = (snode.children || []).some(c => c.type?.startsWith("port:"));
    return {
      "nodeSize.constraints": "NODE_LABELS PORTS PORT_LABELS MINIMUM_SIZE",
      "nodeSize.minimum": "(160, 40)",
      "nodeLabels.placement": hasPorts ? "H_CENTER V_TOP INSIDE" : "H_CENTER V_CENTER INSIDE",
      ...(hasPorts ? {
        "portConstraints": "FIXED_SIDE",
        "portLabels.placement": "INSIDE",
        "portLabels.nextToPortIfPossible": "true",
        "elk.portAlignment.west": "CENTER",
        "elk.portAlignment.east": "CENTER",
        "elk.spacing.labelPortHorizontal": "8",
        "elk.spacing.labelPortVertical": "1",
      } : {}),
    };
  }

  protected override portOptions(sport: SPort, _index: SModelIndex): LayoutOptions {
    const isInput = sport.id.includes(".in.");
    return {
      "port.side": isInput ? "WEST" : "EAST",
    };
  }

  protected override labelOptions(_slabel: SLabel, _index: SModelIndex): LayoutOptions | undefined {
    return undefined;
  }
}
