// Modules available without runtime feature (no tokio/reqwest)
pub mod error;
pub mod graph;
pub mod mermaid;

// Modules requiring the runtime feature (tokio, reqwest, rhai, etc.)
#[cfg(feature = "runtime")]
pub mod adapter;
#[cfg(feature = "runtime")]
pub mod auth;
#[cfg(feature = "runtime")]
pub mod blackboard;
#[cfg(feature = "runtime")]
pub mod debug_server;
#[cfg(feature = "runtime")]
pub mod execute;
#[cfg(feature = "runtime")]
pub mod handlers;
#[cfg(feature = "runtime")]
pub mod registry;
#[cfg(feature = "runtime")]
pub mod scripting;
#[cfg(feature = "runtime")]
pub mod template;
#[cfg(feature = "runtime")]
pub mod validation;

// Always-available re-exports
pub use error::{GraphError, NodeError, PortTypeMismatchInfo};
pub use graph::edge::EdgeData;
pub use graph::metadata::GraphMetadata;
pub use graph::node::{Node, NodeId};
pub use graph::port::Port;
pub use graph::types::{PortType, ResultReducer, Value};
pub use graph::{Graph, Subgraph, SubgraphDirective, SubgraphTopology};
pub use mermaid::{export_mermaid, load_mermaid, MermaidError, MermaidErrors, Span};

// Runtime-only re-exports
#[cfg(feature = "runtime")]
pub use adapter::{
    AdapterCapabilities, AdapterRegistry, AiAdapter, AiRequest, AiResponse, ClaudeCliAdapter,
    ConversationConfig, ConversationHistory, ConversationMessage, MessageRole, MockAdapter,
    TokenUsage, CONVERSATION_HISTORY_KEY,
};
#[cfg(feature = "runtime")]
pub use execute::blackboard::{Blackboard, BlackboardScope};
#[cfg(feature = "runtime")]
pub use execute::control::{evaluate_guard, GuardResult, LoopConfig};
#[cfg(feature = "runtime")]
pub use execute::snapshot::ExecutionSnapshot;
#[cfg(feature = "runtime")]
pub use execute::trace::{ExecutionTrace, RetryRecord, TraceRecord};
#[cfg(feature = "runtime")]
pub use execute::{
    auto_install_auth_registry, sync_handler, validate_graph, BackoffStrategy, CancellationToken,
    ConcurrencyLimits, EventBus, EventBusError, EventSubscriber, ExecutionContext, ExecutionError,
    ExecutionEvent, ExecutionResult, Executor, HandlerKind, HandlerRegistry, HandlerSchema,
    LoopController, LoopIterator, LoopState, NodeHandler, NodeState, Outputs, RetryConfig,
    SchemaField, SteppedExecutor, TickResult, TopologicalExecutor, ValidationIssue,
    ValidationIssueKind, ValidationReport,
};
#[cfg(feature = "runtime")]
pub use handlers::{
    BreakHandler, CatchHandler, DelayHandler, ErrorTransformHandler, FallbackHandler, GateHandler,
    JsonTransformHandler, LlmCallHandler, LogHandler, MergeHandler, PassthroughHandler,
    RetryHandler, RhaiHandler, SelectHandler, ShellHandler, SplitHandler, TransformHandler,
};
#[cfg(feature = "runtime")]
pub use registry::NodeRegistry;
#[cfg(feature = "runtime")]
pub use template::{
    default_resolver, PromptTemplate, PromptTemplateResolver, TemplateError, TemplateResolver,
};
