# Project Structure

<!-- Maintain this file to document the folder structure of the project. -->
<!-- Last updated: 2026-03-28 (Phase 3) -->

```
psflow/
├── Cargo.toml                  — Package manifest (petgraph, serde, thiserror)
├── justfile                    — Task runner (build, test, lint, fmt, etc.)
├── README.md
├── .gitignore
├── examples/
│   ├── dungeon_generator.mmd   — Procedural dungeon generation workflow
│   ├── texture_generator.mmd   — Procedural texture generation workflow
│   ├── mesh_generator.mmd      — Procedural mesh generation workflow
│   └── midi_generator.mmd      — Procedural MIDI generation workflow
├── src/
│   ├── lib.rs                  — Public API re-exports
│   ├── main.rs                 — CLI entry point (stub)
│   ├── error.rs                — NodeError and GraphError hierarchies
│   ├── graph/
│   │   ├── mod.rs              — Graph struct, Subgraph, SubgraphDirective, construction/query, custom serde
│   │   ├── node.rs             — NodeId, Node with builder pattern
│   │   ├── edge.rs             — EdgeData (source_port, target_port, label)
│   │   ├── port.rs             — Port (name + PortType)
│   │   ├── types.rs            — PortType enum, Value enum, type parsing and compatibility
│   │   ├── metadata.rs         — GraphMetadata
│   │   └── validation.rs       — Graph validation (cycles, orphans, type mismatches, missing inputs)
│   ├── execute/
│   │   ├── mod.rs              — Execution module declarations and re-exports (blackboard, control, context, topological)
│   │   ├── blackboard.rs       — Scoped key-value store (BlackboardScope: Global/Subgraph/Node, Blackboard struct with fallback reads)
│   │   ├── context.rs          — ExecutionContext with Blackboard integration, branch decision tracking, and state reset
│   │   ├── control.rs          — Control flow strategies: evaluate_guard(), execute_sequence/parallel/race/loop, GuardResult, LoopConfig
│   │   └── topological.rs      — Topological executor with subgraph delegation, branch decision handling, and blocked-node detection
│   ├── mermaid/
│   │   ├── mod.rs              — MermaidError enum, module re-exports
│   │   ├── parse.rs            — Line-by-line Mermaid parser → ParsedMermaid intermediate representation
│   │   ├── annotation.rs       — Annotation parsing and application to Graph nodes
│   │   ├── loader.rs           — load_mermaid() entry point: parse + annotate + resolve → Graph
│   │   └── export.rs           — export_mermaid(): Graph → annotated Mermaid string
│   ├── adapter/
│   │   ├── mod.rs              — AiAdapter trait, AdapterCapabilities, AiRequest (builder), AiResponse, TokenUsage
│   │   ├── mock.rs             — MockAdapter: pattern→response matching, configurable default, capabilities override
│   │   └── registry.rs         — AdapterRegistry: register, default selection, resolve, capability validation
│   ├── handlers/
│   │   ├── mod.rs              — Built-in handler exports
│   │   └── llm_call.rs         — LlmCallHandler: NodeHandler delegating to AiAdapter via PromptTemplate; transform/oracle modes, json format
│   ├── registry.rs             — NodeRegistry: handler registration, lookup, override, and graph validation
│   └── template.rs             — PromptTemplate: compile/render with {var} interpolation, {#if}...{/if} conditionals, escaped braces
└── ergon/
    ├── project-data/           — Ergon project metadata files
    └── active-documents/       — Active design docs, plans, ADRs
```