# Code Manifest

<!-- Outline of source code folders and files with descriptions. -->
<!-- Keep entries concise — one line per item. -->
<!-- Last updated: 2026-03-28 (Phase 3) -->

## src/

| File | Description |
|------|-------------|
| `lib.rs` | Crate root. Declares `error`, `graph`, `execute`, `mermaid`, `adapter`, `handlers`, `registry`, and `template` modules. Re-exports all public types across all layers. |
| `main.rs` | Binary entry point. Prints version string (stub for future CLI). |
| `error.rs` | Error types using `thiserror`. `NodeError` covers runtime failures (timeout, cancel, type mismatch, adapter error). `GraphError` covers structural issues (cycles, orphans, port type mismatches, duplicates, missing inputs, not-found). |

## src/graph/

| File | Description |
|------|-------------|
| `mod.rs` | Core `Graph` struct backed by `petgraph::StableDiGraph<Node, EdgeData>`. Maintains a `HashMap<NodeId, NodeIndex>` for O(1) node lookup. Provides node CRUD, edge CRUD, predecessor/successor queries, incoming/outgoing edge queries, subgraph management, and metadata access. Implements custom `Serialize`/`Deserialize` that flatten the graph to a `{metadata, nodes[], edges[], subgraphs[]}` JSON representation. Also defines `Subgraph` (node grouping with execution directive) and `SubgraphDirective` enum (None, Parallel, Race, Event, Loop, Named). |
| `node.rs` | `NodeId` newtype wrapper over `String` with `From<&str>` and `Display`. `Node` struct with id, label, optional handler, input/output port lists, and config/exec JSON blobs. Builder-pattern methods: `with_handler`, `with_input`, `with_output`. Port lookup via `input_port`/`output_port`. |
| `edge.rs` | `EdgeData` struct: `source_port`, `target_port`, optional `label`. Stored as edge weights in the petgraph digraph. |
| `port.rs` | `Port` struct: a named connection point on a node with a `PortType`. Constructor: `Port::new(name, port_type)`. |
| `types.rs` | `PortType` enum: `String`, `Bool`, `I64`, `F32`, `Vec(Box<PortType>)`, `Map(Box<PortType>)`, `Domain(String)`, `Any`. Implements `FromStr`, `Display`, `is_compatible_with` for type-checking, and `Value::matches_type(&PortType) -> bool` for runtime token type validation. `Value` enum mirrors PortType for runtime data with tagged serde (`{type, value}`). |
| `metadata.rs` | `GraphMetadata` struct: optional name, version, description, default_executor, required_adapter, author; plus required_capabilities and tags lists. Parsed from `%% @graph` annotations. |
| `validation.rs` | Extends `Graph` with `validate()` and `validate_as_dag()` methods. Checks: orphan nodes (no connections in multi-node graphs), port existence, port type compatibility, missing required inputs, self-loops, and multi-node cycles (via Tarjan SCC). Returns all errors rather than failing on first. |

## src/execute/

| File | Description |
|------|-------------|
| `mod.rs` | Declares `blackboard`, `control`, `context`, and `topological` submodules. Re-exports all execute-layer public types. |
| `blackboard.rs` | `BlackboardScope` enum (Global, Subgraph, Node). `Blackboard` struct with scoped `get`/`set`/`remove`/`clear_scope`. Reads fall back from Node → Subgraph → Global scope. |
| `context.rs` | `ExecutionContext` extended with an embedded `Blackboard` and branch decision map. Adds `blackboard()`, `set_branch_decision()`, `get_branch_decision()`, and `reset_states()` methods. |
| `control.rs` | Control flow execution strategies: `evaluate_guard()` for branch condition evaluation, `execute_sequence`/`parallel`/`race`/`loop` for subgraph execution patterns. Defines `GuardResult` and `LoopConfig` enums. |
| `topological.rs` | Topological executor extended with subgraph-awareness. Detects `SubgraphDirective` and delegates to control flow strategies. Handles branch decisions via `handle_branch_decision` and `is_branch_blocked`. `collect_inputs` is `pub(crate)`. |

## src/mermaid/

| File | Description |
|------|-------------|
| `mod.rs` | `MermaidError` enum for parsing/annotation errors. Module re-exports for `parse`, `annotation`, `loader`, and `export` submodules. |
| `parse.rs` | Line-by-line Mermaid parser producing `ParsedMermaid` intermediate representation with nodes, edges, subgraphs, and raw annotation lines. Uses `nom` combinator parsing. No dependency on `Graph` — purely syntactic. |
| `annotation.rs` | Annotation value parsing (`%% @NodeID key.path: value`) with dot-path expansion into nested JSON. Validates reserved keys (handler, inputs, outputs, config, exec, *_llm). Applies extracted annotations to `Graph` nodes, populating handler, port definitions, and config/exec fields. |
| `loader.rs` | Entry point `load_mermaid(path: &Path) -> Result<Graph>`. Chains: parse → extract annotations → apply to graph → port resolution → subgraph directives → return fully typed, executable `Graph`. |
| `export.rs` | `export_mermaid(graph: &Graph) -> String`. Serializes `Graph` back to valid Mermaid `.mmd` with embedded `%%` annotations for configuration. Supports round-trip: export → re-import → structurally equivalent graph. |

## src/adapter/

| File | Description |
|------|-------------|
| `mod.rs` | `AiAdapter` trait with `call` and `capabilities` methods. `AdapterCapabilities` with `satisfies`/`missing` helpers. `AiRequest` builder, `AiResponse`, and `TokenUsage` types. |
| `mock.rs` | `MockAdapter`: pattern-matched responses keyed by prompt substring, configurable default response, and capabilities override. Used in tests. |
| `registry.rs` | `AdapterRegistry`: register named adapters, set/get default, resolve by name or default, validate capability requirements. |

## src/handlers/

| File | Description |
|------|-------------|
| `mod.rs` | Re-exports all built-in `NodeHandler` implementations. |
| `llm_call.rs` | `LlmCallHandler`: `NodeHandler` that resolves an adapter, renders a `PromptTemplate` from node config, calls the adapter, and writes output. Supports `transform`/`oracle` execution modes and `json` output format. |

## src/

| File | Description |
|------|-------------|
| `registry.rs` | `NodeRegistry`: register/lookup/override `NodeHandler` factories by name. Provides `validate_graph` to check all graph nodes have registered handlers. |
| `template.rs` | `PromptTemplate`: compile-time parsing and render-time interpolation of `{var}`, `{inputs.*}`, `{ctx.*}` placeholders; `{#if var}...{/if}` conditional blocks; `{{`/`}}` escape sequences. |

## examples/

| File | Description |
|------|-------------|
| `dungeon_generator.mmd` | Mermaid flowchart defining a procedural dungeon generation workflow. |
| `texture_generator.mmd` | Mermaid flowchart defining a procedural texture generation workflow. |
| `mesh_generator.mmd` | Mermaid flowchart defining a procedural mesh generation workflow. |
| `midi_generator.mmd` | Mermaid flowchart defining a procedural MIDI generation workflow. |