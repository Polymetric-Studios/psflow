# Architecture

<!-- High-level mermaid diagram of module relationships and data flow. -->
<!-- Keep this diagram accurate as the codebase evolves. -->
<!-- Last updated: 2026-03-28 -->

## Module Relationships

```mermaid
flowchart TD
    lib["lib.rs<br/>(public API re-exports)"]
    main["main.rs<br/>(CLI stub)"]
    error["error.rs<br/>NodeError, GraphError"]
    graph_mod["graph/mod.rs<br/>Graph, Subgraph, SubgraphDirective<br/>custom Serialize/Deserialize"]
    node["graph/node.rs<br/>NodeId, Node"]
    edge["graph/edge.rs<br/>EdgeData"]
    port["graph/port.rs<br/>Port"]
    types["graph/types.rs<br/>PortType, Value"]
    metadata["graph/metadata.rs<br/>GraphMetadata"]
    validation["graph/validation.rs<br/>validate, validate_as_dag"]
    petgraph["petgraph::StableDiGraph"]

    lib --> graph_mod
    lib --> error
    main -.-> lib

    graph_mod --> node
    graph_mod --> edge
    graph_mod --> port
    graph_mod --> types
    graph_mod --> metadata
    graph_mod --> petgraph

    validation --> graph_mod
    validation --> error
    validation --> petgraph

    node --> port
    port --> types
    error --> types
```

## Key Design Decisions

1. **petgraph::StableDiGraph as backing store** -- Stable indices survive node/edge removal, which matters for incremental graph editing. Nodes carry `Node` weights; edges carry `EdgeData` weights.

2. **NodeId-to-NodeIndex map** -- `HashMap<NodeId, NodeIndex>` provides O(1) lookup by string ID while petgraph operates on integer indices internally.

3. **Custom serde** -- `Graph` implements `Serialize`/`Deserialize` manually to flatten the internal petgraph representation into a portable `{metadata, nodes[], edges[], subgraphs[]}` JSON format. Deserialization rebuilds the petgraph and node map.

4. **Validation collects all errors** -- `validate()` and `validate_as_dag()` return `Vec<GraphError>` rather than short-circuiting, so callers see every problem in one pass.

5. **Type compatibility is structural** -- `PortType::is_compatible_with` checks exact match, Any wildcard, i64-to-f32 coercion, and recursive Vec/Map element compatibility. Domain types match by name only.