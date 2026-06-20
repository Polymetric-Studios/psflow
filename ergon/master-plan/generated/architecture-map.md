# Architecture map (import graph)

The static import graph — an import-reality cross-check, NOT the architecture.
Low-signal on its own (utility-edge dominated, blind to dynamic indirection);
read the curated architecture narrative for layers, boundaries, and intent.
Resolution is per-language and coarse: Rust nodes are crates; other languages
group by top-level directory and resolve imports to a head segment. The graph is
**Rust-primary, best-effort elsewhere** — non-Rust internal (relative) imports
are not yet resolved to edges (flagged per run when present), so the non-Rust
graph may understate internal structure; full per-ecosystem module resolution is
a refinement. Do not hand-edit: regenerate.

granularity: crate

edges:
  debugger -> @codemirror
  debugger -> child_process
  debugger -> elkjs
  debugger -> fs
  debugger -> inversify
  debugger -> os
  debugger -> path
  debugger -> reflect-metadata
  debugger -> snabbdom
  debugger -> sprotty
  debugger -> sprotty-elk
  debugger -> sprotty-protocol
  debugger -> url
  debugger -> vite
  debugger -> vite-plugin-wasm
  debugger -> vitest
  debugger -> web-worker
  psflow -> accumulator
  psflow -> adapter
  psflow -> anthropic_api
  psflow -> apply_ctx
  psflow -> async_trait
  psflow -> bearer
  psflow -> blackboard
  psflow -> clap
  psflow -> claude_cli
  psflow -> claude_terminal
  psflow -> claude_workflow
  psflow -> composio
  psflow -> concurrency
  psflow -> context
  psflow -> control
  psflow -> conversation
  psflow -> cookie_jar
  psflow -> decl
  psflow -> edge
  psflow -> error
  psflow -> event
  psflow -> event_bus
  psflow -> event_driven
  psflow -> execute
  psflow -> export
  psflow -> file_io
  psflow -> futures
  psflow -> graph
  psflow -> handlers
  psflow -> hmac
  psflow -> http
  psflow -> human_input
  psflow -> json_transform
  psflow -> lifecycle
  psflow -> llm_call
  psflow -> loader
  psflow -> loop_controller
  psflow -> loop_handler
  psflow -> map
  psflow -> mermaid
  psflow -> metadata
  psflow -> mock
  psflow -> node
  psflow -> openai_compat
  psflow -> parse
  psflow -> petgraph
  psflow -> poll_until
  psflow -> portable_pty
  psflow -> reactive
  psflow -> registry
  psflow -> reqwest
  psflow -> resolver
  psflow -> retry
  psflow -> rhai
  psflow -> rhai_handler
  psflow -> secret
  psflow -> serde
  psflow -> sha2
  psflow -> shell
  psflow -> state
  psflow -> static_header
  psflow -> stepped
  psflow -> strategies
  psflow -> strategy
  psflow -> subgraph_invoke
  psflow -> template
  psflow -> thiserror
  psflow -> tokio
  psflow -> tokio_tungstenite
  psflow -> tokio_util
  psflow -> topological
  psflow -> tracing
  psflow -> tracing_subscriber
  psflow -> utility
  psflow -> validation
  psflow -> websocket
  psflow -> wiremock
  psflow -> zeroize
  psflow_wasm -> psflow
  psflow_wasm -> serde
  psflow_wasm -> tsify_next
  psflow_wasm -> wasm_bindgen
  scripts -> @composio

flags:
  - language-prelude / stdlib imports omitted as noise
  - non-Rust relative/internal imports are not resolved to edges — this graph is Rust-primary; non-Rust internal structure is best-effort and may be understated
