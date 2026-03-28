# Code Manifest

<!-- Outline of source code folders and files with descriptions. -->
<!-- Keep entries concise — one line per item. -->
<!-- Last updated: 2026-03-28 -->

## src/

| File | Description |
|------|-------------|
| `lib.rs` | Crate root. Declares `error` and `graph` modules. Re-exports all public types: `Graph`, `Subgraph`, `SubgraphDirective`, `Node`, `NodeId`, `EdgeData`, `Port`, `PortType`, `Value`, `GraphMetadata`, `GraphError`, `NodeError`. |
| `main.rs` | Binary entry point. Prints version string (stub for future CLI). |
| `error.rs` | Error types using `thiserror`. `NodeError` covers runtime failures (timeout, cancel, type mismatch, adapter error). `GraphError` covers structural issues (cycles, orphans, port type mismatches, duplicates, missing inputs, not-found). |

## src/graph/

| File | Description |
|------|-------------|
| `mod.rs` | Core `Graph` struct backed by `petgraph::StableDiGraph<Node, EdgeData>`. Maintains a `HashMap<NodeId, NodeIndex>` for O(1) node lookup. Provides node CRUD, edge CRUD, predecessor/successor queries, incoming/outgoing edge queries, subgraph management, and metadata access. Implements custom `Serialize`/`Deserialize` that flatten the graph to a `{metadata, nodes[], edges[], subgraphs[]}` JSON representation. Also defines `Subgraph` (node grouping with execution directive) and `SubgraphDirective` enum (None, Parallel, Race, Event, Loop, Named). |
| `node.rs` | `NodeId` newtype wrapper over `String` with `From<&str>` and `Display`. `Node` struct with id, label, optional handler, input/output port lists, and config/exec JSON blobs. Builder-pattern methods: `with_handler`, `with_input`, `with_output`. Port lookup via `input_port`/`output_port`. |
| `edge.rs` | `EdgeData` struct: `source_port`, `target_port`, optional `label`. Stored as edge weights in the petgraph digraph. |
| `port.rs` | `Port` struct: a named connection point on a node with a `PortType`. Constructor: `Port::new(name, port_type)`. |
| `types.rs` | `PortType` enum: `String`, `Bool`, `I64`, `F32`, `Vec(Box<PortType>)`, `Map(Box<PortType>)`, `Domain(String)`, `Any`. Implements `FromStr` for parsing type strings (e.g. `"Room[]"`, `"Map<Room>"`), `Display` for round-tripping, and `is_compatible_with` for type-checking connections (exact match, Any wildcard, i64-to-f32 coercion, recursive Vec/Map). `Value` enum mirrors PortType for runtime data with tagged serde (`{type, value}`). |
| `metadata.rs` | `GraphMetadata` struct: optional name, version, description, default_executor, required_adapter, author; plus required_capabilities and tags lists. Parsed from `%% @graph` annotations. |
| `validation.rs` | Extends `Graph` with `validate()` and `validate_as_dag()` methods. Checks: orphan nodes (no connections in multi-node graphs), port existence, port type compatibility, missing required inputs, self-loops, and multi-node cycles (via Tarjan SCC). Returns all errors rather than failing on first. |

## examples/

| File | Description |
|------|-------------|
| `dungeon_generator.mmd` | Mermaid flowchart defining a procedural dungeon generation workflow. |
| `texture_generator.mmd` | Mermaid flowchart defining a procedural texture generation workflow. |
| `mesh_generator.mmd` | Mermaid flowchart defining a procedural mesh generation workflow. |
| `midi_generator.mmd` | Mermaid flowchart defining a procedural MIDI generation workflow. |