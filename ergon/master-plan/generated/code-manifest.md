# Code manifest

Generated deterministically from source (structure only). Each file's `purpose`
is its module-level doc comment (e.g. Rust `//!`, Python module docstring)
— authored, not generated. `flags` mark what is not statically resolvable. Do
not hand-edit: regenerate.

## crates/psflow-wasm/src/lib.rs
purpose: (none)
exports:
  - struct ParseResult
  - struct EdgeRange
  - struct NodeRange
  - struct AnnotationRange
  - struct SubgraphRange
  - struct SpanDto
  - fn parse_mmd
  - struct TraceEvent
  - struct TraceResult
  - fn parse_trace
flags:
  - glob import unresolved: wasm_bindgen::prelude::*

## debugger/dist/assets/index-BIxFWVHY.js
purpose: (none)
exports: (none)

## debugger/pkg/psflow_wasm.d.ts
purpose: tslint:disable
exports:
  - interface TraceEvent
  - interface TraceResult
  - interface ParseResult
  - interface AnnotationRange
  - interface EdgeRange
  - interface NodeRange
  - interface SpanDto
  - interface SubgraphRange
  - type InitInput
  - interface InitOutput
  - type SyncInitInput
  - default __wbg_init

## debugger/pkg/psflow_wasm.js
purpose: @ts-self-types="./psflow_wasm.d.ts"
exports:
  - function parse_mmd
  - function parse_trace

## debugger/pkg/psflow_wasm_bg.wasm.d.ts
purpose: tslint:disable
exports:
  - const memory
  - const parse_mmd
  - const parse_trace
  - const __wbindgen_malloc
  - const __wbindgen_realloc
  - const __wbindgen_externrefs
  - const __externref_table_dealloc
  - const __wbindgen_start

## debugger/src/editor.ts
purpose: (none)
exports:
  - const setNodeStates
  - const setSelectedNode
  - const setParseResult
  - const setTraceResult
  - const setTracePosition
  - const setBreakpoints
  - interface EditorHandle
  - function createEditor

## debugger/src/graph/di-config.ts
purpose: Sprotty Inversify container configuration.
exports:
  - function createContainer

## debugger/src/graph/index.ts
purpose: Graph visualization panel — ELK layout + Sprotty rendering.
exports:
  - interface GraphHandle
  - function createGraph

## debugger/src/graph/layout-store.ts
purpose: Shared store for ELK layout data.
exports:
  - interface LayoutData
  - function setLayoutData
  - function getLayoutData
  - function extractLayoutData

## debugger/src/graph/layout.ts
purpose: ELK layout configuration — maps Sprotty element types to ELK layout options.
exports:
  - class PsflowLayoutConfigurator

## debugger/src/graph/listeners.ts
purpose: Sprotty event listeners — double-click, selection, keyboard.
exports:
  - const PSFLOW_CALLBACKS
  - interface PsflowCallbacks
  - class PsflowMouseListener
  - class PsflowSelectionListener
  - class PsflowKeyListener

## debugger/src/graph/model.ts
purpose: Sprotty model types and ParseResult → SGraph builder.
exports:
  - const GRAPH
  - const NODE
  - const SUBGRAPH
  - const EDGE
  - const PORT
  - const LABEL_NODE
  - const LABEL_EDGE
  - const LABEL_PORT
  - const LABEL_SUBGRAPH
  - function measureLabel
  - function buildSprottyModel

## debugger/src/graph/views.ts
purpose: Sprotty SVG views — renders each model element type.
exports:
  - class PsflowGraphView
  - class PsflowNodeView
  - class PsflowSubgraphView
  - class PsflowEdgeView
  - class PsflowPortView
  - class PsflowLabelView

## debugger/src/inspector.ts
purpose: (none)
exports:
  - function setInspectorOnUpdate
  - function renderInspector

## debugger/src/live.test.ts
purpose: (none)
exports: (none)

## debugger/src/live.ts
purpose: Live WebSocket connection to a running psflow debug server.
exports:
  - type LiveStatus
  - interface LiveCallbacks
  - interface LiveConnection
  - function applyDebugEvents
  - function connectLive

## debugger/src/main.ts
purpose: (none)
exports: (none)

## debugger/src/playback.ts
purpose: (none)
exports:
  - interface PlaybackController
  - function createPlayback

## debugger/src/state.test.ts
purpose: (none)
exports: (none)

## debugger/src/state.ts
purpose: (none)
exports:
  - type NodeState
  - interface DebuggerState
  - function createState
  - function saveBreakpoints
  - function deriveNodeStates
  - function getNodeEvent
  - function findNodeRange

## debugger/src/timeline.ts
purpose: (none)
exports:
  - function destroyTimeline
  - function initTimeline
  - function updateTimeline

## debugger/src/wasm.ts
purpose: (none)
exports:
  - function initWasm

## debugger/vite.config.ts
purpose: (none)
exports:
  - default default

## examples/pty_approve.rs
purpose: Phase C live check — force an approval dialog (write-tool in `default`
exports: (none)

## examples/pty_capture.rs
purpose: Capture real `claude` TUI screens across states (idle / working / approval
exports: (none)

## examples/pty_notify.rs
purpose: Verify dialog *routing*: force an approval dialog, route it through the
exports: (none)

## examples/pty_spike.rs
purpose: PTY driver demo — drives the real interactive `claude` TUI headless via the
exports: (none)

## examples/pty_transcript.rs
purpose: Spike: transcript-based turn-completion. Drives a tool-using turn and detects
exports: (none)

## scripts/trigger_create.mjs
purpose: (none)
exports: (none)

## scripts/triggers_listen.mjs
purpose: (none)
exports: (none)

## src/adapter/anthropic_api.rs
purpose: Direct HTTP adapter for the Anthropic Messages API (`/v1/messages`).
exports:
  - struct AnthropicApiAdapter

## src/adapter/claude_cli.rs
purpose: (none)
exports:
  - struct ClaudeCliAdapter

## src/adapter/claude_terminal.rs
purpose: Drive the real interactive `claude` TUI headless over a pseudo-terminal.
exports:
  - enum TerminalError
  - enum Key
  - struct SessionOptions
  - enum ResultSource
  - struct TurnResult
  - type ApprovalNotifier
  - struct ApprovalPrompt
  - enum ApprovalChoice
  - enum ApprovalPolicy
  - fn detect_approval
  - struct ClaudeTerminalSession

## src/adapter/conversation.rs
purpose: Conversation history for LLM context accumulation.
exports:
  - enum MessageRole
  - struct ConversationMessage
  - struct ConversationConfig
  - struct ConversationHistory
  - const CONVERSATION_HISTORY_KEY

## src/adapter/mock.rs
purpose: (none)
exports:
  - struct MockAdapter

## src/adapter/mod.rs
purpose: (none)
exports:
  - mod anthropic_api
  - mod claude_cli
  - mod claude_terminal
  - mod conversation
  - mod mock
  - mod openai_compat
  - mod registry
  - struct AdapterCapabilities
  - enum CacheControl
  - struct PromptBlock
  - struct AiRequest
  - struct AiResponse
  - struct TokenUsage
  - trait AiAdapter
flags:
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here

## src/adapter/openai_compat.rs
purpose: Generic adapter for OpenAI-compatible Chat Completions endpoints.
exports:
  - struct OpenAiCompatAdapter

## src/adapter/registry.rs
purpose: (none)
exports:
  - struct AdapterRegistry

## src/auth/apply_ctx.rs
purpose: (none)
exports:
  - struct AuthApplyCtx

## src/auth/decl.rs
purpose: Re-export of the shared [`AuthStrategyDecl`] from the always-available
exports: (none)
flags:
  - re-export (pub use): names republished, not resolved here

## src/auth/error.rs
purpose: (none)
exports:
  - enum SecretError
  - enum AuthError

## src/auth/mod.rs
purpose: Graph-scoped authentication strategy layer.
exports: (none)
flags:
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here

## src/auth/registry.rs
purpose: (none)
exports:
  - type AuthStrategyFactory
  - struct AuthStrategyRegistry

## src/auth/resolver.rs
purpose: (none)
exports:
  - struct SecretRequest
  - trait SecretResolver
  - struct NullSecretResolver
  - struct StaticSecretResolver

## src/auth/secret.rs
purpose: (none)
exports:
  - struct SecretValue

## src/auth/state.rs
purpose: (none)
exports:
  - struct AuthState
  - struct CookieJar

## src/auth/strategies/bearer.rs
purpose: (none)
exports:
  - const BEARER_TYPE
  - struct BearerStrategy

## src/auth/strategies/cookie_jar.rs
purpose: (none)
exports:
  - const COOKIE_JAR_TYPE
  - struct CookieJarStrategy
  - fn domain_matches

## src/auth/strategies/hmac.rs
purpose: (none)
exports:
  - const HMAC_TYPE
  - struct HmacStrategy

## src/auth/strategies/mod.rs
purpose: Built-in auth strategies: static_header, bearer, hmac, cookie_jar.
exports:
  - fn register_builtins
flags:
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here

## src/auth/strategies/static_header.rs
purpose: (none)
exports:
  - const STATIC_HEADER_TYPE
  - struct StaticHeaderStrategy

## src/auth/strategy.rs
purpose: (none)
exports:
  - trait AuthStrategy

## src/bin/composio.rs
purpose: Minimal host runner for Composio-backed psflow graphs.
exports: (none)

## src/bin/manifest.rs
purpose: Emit a JSON manifest of psflow's built-in handlers.
exports: (none)

## src/bin/psflow_run.rs
purpose: psflow-run — personal runner for named psflow graphs.
exports: (none)

## src/blackboard/helpers.rs
purpose: Opinionated helpers over [`Blackboard`] for common workflow conventions.
exports:
  - const WORKFLOW_INPUTS
  - const WORKFLOW_CONSTANTS
  - const WORKFLOW_RESULTS
  - const WORKFLOW_LOOP_STACK
  - const WORKFLOW_OUTPUT_DIR
  - const PROMOTED_PREFIX
  - const LOOP_BREAK
  - struct LoopVars
  - fn write_map
  - fn read_map
  - fn init
  - struct WorkflowStateView
  - fn build_context_maps
  - fn set_output_dir
  - fn set_result
  - fn promoted_keys
  - fn get_value
  - fn push_loop_vars
  - fn pop_loop_vars
  - fn update_loop_vars
  - fn has_break_signal
  - fn clear_break_signal
  - fn set_break_signal

## src/blackboard/mod.rs
purpose: Public blackboard surface.
exports:
  - mod helpers
flags:
  - re-export (pub use): names republished, not resolved here

## src/debug_server.rs
purpose: WebSocket debug server for live step-through debugging.
exports:
  - fn run_debug_server

## src/error.rs
purpose: (none)
exports:
  - enum NodeError
  - struct PortTypeMismatchInfo
  - enum GraphError

## src/execute/blackboard.rs
purpose: (none)
exports:
  - enum BlackboardScope
  - enum ContextInheritance
  - struct Blackboard
  - struct BlackboardSnapshot

## src/execute/concurrency.rs
purpose: (none)
exports:
  - struct ConcurrencyLimits
  - fn subgraph_semaphore

## src/execute/context.rs
purpose: (none)
exports:
  - struct ExecutionContext
flags:
  - re-export (pub use): names republished, not resolved here

## src/execute/control.rs
purpose: (none)
exports:
  - const DEFAULT_MAX_LOOP_ITERATIONS
  - enum GuardResult
  - fn evaluate_guard
  - fn evaluate_guard_llm
  - fn evaluate_race_criterion_llm
  - fn evaluate_loop_condition_llm
  - enum LoopConfig
  - fn execute_sequence
  - fn execute_parallel
  - fn execute_race
  - fn execute_race_with_adapter
  - fn execute_loop
  - fn execute_loop_with_adapter
  - fn execute_parallel_loop

## src/execute/event.rs
purpose: (none)
exports:
  - enum ExecutionEvent

## src/execute/event_bus.rs
purpose: (none)
exports:
  - struct EventBus
  - struct EventSubscriber
  - enum EventBusError

## src/execute/event_driven.rs
purpose: (none)
exports:
  - struct EventMessage
  - struct EventSender
  - struct EventDrivenExecutor

## src/execute/lifecycle.rs
purpose: (none)
exports:
  - enum NodeState

## src/execute/loop_controller.rs
purpose: (none)
exports:
  - trait LoopIterator
  - struct LoopState
  - struct LoopController

## src/execute/mod.rs
purpose: (none)
exports:
  - mod blackboard
  - mod concurrency
  - mod context
  - mod control
  - mod event
  - mod event_bus
  - mod event_driven
  - mod lifecycle
  - mod loop_controller
  - mod reactive
  - mod retry
  - mod snapshot
  - mod stepped
  - mod topological
  - mod trace
  - mod validation
  - type Outputs
  - type HandlerRegistry
  - enum HandlerKind
  - struct HandlerSchema
  - struct SchemaField
  - trait NodeHandler
  - trait Executor
  - struct ExecutionResult
  - enum ExecutionError
  - fn auto_install_auth_registry
  - fn sync_handler
flags:
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here

## src/execute/reactive.rs
purpose: (none)
exports:
  - struct ReactiveExecutor

## src/execute/retry.rs
purpose: (none)
exports:
  - enum BackoffStrategy
  - struct RetryConfig
  - fn execute_with_retry
  - fn execute_with_retry_ctx

## src/execute/snapshot.rs
purpose: Execution snapshots for checkpoint/resume of long-running workflows.
exports:
  - struct ExecutionSnapshot

## src/execute/stepped.rs
purpose: (none)
exports:
  - struct TickResult
  - struct SteppedExecutor

## src/execute/topological.rs
purpose: (none)
exports:
  - struct TopologicalExecutor

## src/execute/trace.rs
purpose: Execution trace — structured record of a graph execution for replay and debugging.
exports:
  - struct ExecutionTrace
  - struct TraceRecord
  - struct RetryRecord

## src/execute/validation.rs
purpose: Graph-load validation pass.
exports:
  - enum ValidationIssueKind
  - struct ValidationIssue
  - struct ValidationReport
  - fn validate_graph

## src/graph/auth_decl.rs
purpose: Serializable declaration of a graph-scoped auth strategy.
exports:
  - struct AuthStrategyDecl

## src/graph/edge.rs
purpose: (none)
exports:
  - struct EdgeData

## src/graph/metadata.rs
purpose: (none)
exports:
  - struct GraphMetadata

## src/graph/mod.rs
purpose: (none)
exports:
  - mod auth_decl
  - mod edge
  - mod metadata
  - mod node
  - mod port
  - mod types
  - enum SubgraphDirective
  - struct Subgraph
  - struct SubgraphTopology
  - struct Graph

## src/graph/node.rs
purpose: (none)
exports:
  - struct NodeId
  - struct Node

## src/graph/port.rs
purpose: (none)
exports:
  - struct Port

## src/graph/types.rs
purpose: (none)
exports:
  - enum PortType
  - enum Value
  - enum ResultReducer

## src/graph/validation.rs
purpose: (none)
exports: (none)

## src/handlers/accumulator.rs
purpose: (none)
exports:
  - struct AccumulatorHandler

## src/handlers/claude_workflow.rs
purpose: `claude_workflow` handler — run a prompt (often a Claude Code dynamic
exports:
  - struct ClaudeWorkflowHandler

## src/handlers/common.rs
purpose: (none)
exports: (none)

## src/handlers/composio.rs
purpose: Composio handler — execute a Composio tool through the `composio` CLI.
exports:
  - struct ComposioHandler

## src/handlers/control.rs
purpose: Workflow control handlers: `break` and `select`.
exports:
  - struct BreakHandler
  - struct SelectHandler

## src/handlers/error.rs
purpose: (none)
exports:
  - struct CatchHandler
  - struct FallbackHandler
  - struct ErrorTransformHandler
  - struct RetryHandler

## src/handlers/file_io.rs
purpose: (none)
exports:
  - struct ReadFileHandler
  - struct WriteFileHandler
  - struct GlobHandler

## src/handlers/http.rs
purpose: (none)
exports:
  - struct HttpHandler

## src/handlers/human_input.rs
purpose: (none)
exports:
  - struct HumanPrompt
  - struct HumanResponder
  - struct HumanInputReceiver
  - struct HumanInputHandler

## src/handlers/json_transform.rs
purpose: JSON transformation handler — extract and shape JSON values via JMESPath.
exports:
  - struct JsonTransformHandler

## src/handlers/llm_call.rs
purpose: (none)
exports:
  - const CACHE_BOUNDARY_SENTINEL
  - struct LlmCallHandler

## src/handlers/loop_handler.rs
purpose: `loop` handler — accumulating loop over a subgraph (generalizes `poll_until`).
exports:
  - const LOOP_HANDLER_NAME
  - struct LoopHandler
  - struct LoopRegistrySlot

## src/handlers/map.rs
purpose: `map` handler — data-driven fan-out over a runtime list.
exports:
  - struct MapHandler

## src/handlers/mod.rs
purpose: (none)
exports:
  - mod accumulator
  - mod claude_workflow
  - mod composio
  - mod control
  - mod error
  - mod file_io
  - mod http
  - mod human_input
  - mod json_transform
  - mod llm_call
  - mod loop_handler
  - mod map
  - mod poll_until
  - mod rhai_handler
  - mod shell
  - mod subgraph_invoke
  - mod utility
  - mod websocket
flags:
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here

## src/handlers/poll_until.rs
purpose: `poll_until` node handler.
exports:
  - const POLL_UNTIL_HANDLER_NAME
  - struct PollUntilHandler
  - struct PollUntilRegistrySlot

## src/handlers/rhai_handler.rs
purpose: Rhai script handler — executes inline or external `.rhai` scripts as node handlers.
exports:
  - struct RhaiHandler

## src/handlers/shell.rs
purpose: Shell command handler — executes external processes.
exports:
  - struct ShellHandler

## src/handlers/subgraph_invoke.rs
purpose: (none)
exports:
  - struct GraphLibrary
  - struct SubgraphInvocationHandler
  - struct HandlerRegistrySlot

## src/handlers/utility.rs
purpose: (none)
exports:
  - struct PassthroughHandler
  - struct TransformHandler
  - struct DelayHandler
  - struct LogHandler
  - struct MergeHandler
  - struct SplitHandler
  - struct GateHandler

## src/handlers/websocket.rs
purpose: WebSocket node handler.
exports:
  - const WS_HANDLER_NAME
  - struct WebSocketHandler

## src/lib.rs
purpose: (none)
exports:
  - mod error
  - mod graph
  - mod mermaid
  - mod adapter
  - mod auth
  - mod blackboard
  - mod debug_server
  - mod execute
  - mod handlers
  - mod registry
  - mod scripting
  - mod template
  - mod validation
flags:
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here

## src/main.rs
purpose: (none)
exports: (none)

## src/mermaid/annotation.rs
purpose: (none)
exports:
  - fn parse_value
  - fn apply_annotations

## src/mermaid/export.rs
purpose: (none)
exports:
  - fn export_mermaid

## src/mermaid/loader.rs
purpose: (none)
exports:
  - fn load_mermaid

## src/mermaid/mod.rs
purpose: (none)
exports:
  - mod annotation
  - mod export
  - mod loader
  - mod parse
  - enum MermaidError
  - struct MermaidErrors
flags:
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here
  - re-export (pub use): names republished, not resolved here

## src/mermaid/parse.rs
purpose: (none)
exports:
  - struct Span
  - enum Direction
  - enum NodeShape
  - struct ParsedNode
  - struct ParsedEdge
  - struct ParsedSubgraph
  - struct ParsedAnnotation
  - struct ParsedMermaid
  - fn parse

## src/registry.rs
purpose: (none)
exports:
  - struct NodeRegistry

## src/scripting/bridge.rs
purpose: Bidirectional conversion between psflow `Value` and Rhai `Dynamic`.
exports:
  - fn value_to_dynamic
  - fn dynamic_to_value
  - fn outputs_to_rhai_map
  - fn rhai_map_to_outputs

## src/scripting/engine.rs
purpose: Sandboxed Rhai script engine for psflow.
exports:
  - struct ScriptEngineConfig
  - struct ScriptEngine
  - enum ScriptError
  - fn default_script_engine

## src/scripting/mod.rs
purpose: (none)
exports:
  - mod bridge
  - mod engine

## src/template.rs
purpose: (none)
exports:
  - enum TemplateError
  - struct PromptTemplate
  - trait TemplateResolver
  - struct PromptTemplateResolver
  - fn default_resolver

## src/validation/mod.rs
purpose: Transport-agnostic JSON Schema validation.
exports:
  - enum FailureMode
  - struct ValidationConfig
  - enum ValidationConfigError
  - struct ValidationFailure
  - enum ValidationOutcome
  - struct CompiledValidator

## tests/auth.rs
purpose: Integration tests for the graph-scoped auth strategy layer.
exports: (none)

## tests/body_sink.rs
purpose: Integration tests for `body_sink` — streaming HTTP response bodies to
exports: (none)

## tests/cli.rs
purpose: CLI integration tests — verify the psflow binary works end-to-end.
exports: (none)

## tests/poll_until.rs
purpose: Integration tests for the `poll_until` node handler.
exports: (none)

## tests/validate_graph.rs
purpose: Integration tests for the graph-load validation pass.
exports: (none)

## tests/validation.rs
purpose: Integration tests for HTTP response JSON Schema validation.
exports: (none)

## tests/websocket.rs
purpose: Integration tests for the WebSocket node handler.
exports: (none)
