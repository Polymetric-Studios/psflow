/**
 * Sprotty Inversify container configuration.
 *
 * Sprotty is used purely for rendering — ELK layout runs separately
 * before the model is passed to Sprotty with positions pre-set.
 */

import "reflect-metadata";
import { Container, ContainerModule } from "inversify";
import {
  configureModelElement,
  configureViewerOptions,
  loadDefaultModules,
  LocalModelSource,
  SGraphImpl,
  SNodeImpl,
  SEdgeImpl,
  SPortImpl,
  SLabelImpl,
  TYPES,
} from "sprotty";
import {
  GRAPH, NODE, SUBGRAPH, EDGE, PORT,
  LABEL_NODE, LABEL_EDGE, LABEL_PORT, LABEL_SUBGRAPH,
} from "./model.js";
import {
  PsflowGraphView, PsflowNodeView, PsflowSubgraphView,
  PsflowEdgeView, PsflowPortView, PsflowLabelView,
} from "./views.js";
import {
  PsflowMouseListener, PsflowSelectionListener, PsflowKeyListener,
  PSFLOW_CALLBACKS, type PsflowCallbacks,
} from "./listeners.js";

export function createContainer(
  containerId: string,
  callbacks: PsflowCallbacks,
): Container {
  const psflowModule = new ContainerModule((bind, unbind, isBound, rebind) => {
    const context = { bind, unbind, isBound, rebind };

    // Model source — no layout engine, positions are pre-computed
    bind(TYPES.ModelSource).to(LocalModelSource).inSingletonScope();

    // Callbacks service
    bind(PSFLOW_CALLBACKS).toConstantValue(callbacks);

    // Event listeners
    bind(PsflowMouseListener).toSelf().inSingletonScope();
    bind(TYPES.MouseListener).toService(PsflowMouseListener);
    bind(PsflowSelectionListener).toSelf().inSingletonScope();
    bind(TYPES.MouseListener).toService(PsflowSelectionListener);
    bind(PsflowKeyListener).toSelf().inSingletonScope();
    bind(TYPES.KeyListener).toService(PsflowKeyListener);

    // Model element → View mappings
    configureModelElement(context, GRAPH, SGraphImpl, PsflowGraphView);
    configureModelElement(context, NODE, SNodeImpl, PsflowNodeView);
    configureModelElement(context, SUBGRAPH, SNodeImpl, PsflowSubgraphView);
    configureModelElement(context, EDGE, SEdgeImpl, PsflowEdgeView);
    configureModelElement(context, PORT, SPortImpl, PsflowPortView);
    configureModelElement(context, LABEL_NODE, SLabelImpl, PsflowLabelView);
    configureModelElement(context, LABEL_EDGE, SLabelImpl, PsflowLabelView);
    configureModelElement(context, LABEL_PORT, SLabelImpl, PsflowLabelView);
    configureModelElement(context, LABEL_SUBGRAPH, SLabelImpl, PsflowLabelView);

    // No layout — positions baked into model before setModel
    configureViewerOptions(context, {
      needsClientLayout: false,
      needsServerLayout: false,
      baseDiv: containerId,
    });
  });

  const container = new Container();
  loadDefaultModules(container);
  container.load(psflowModule);
  return container;
}
