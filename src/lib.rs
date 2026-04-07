// Modules available without runtime feature (no tokio/reqwest)
pub mod error;
pub mod graph;
pub mod mermaid;

// Modules requiring the runtime feature (tokio, reqwest, rhai, etc.)
#[cfg(feature = "runtime")]
pub mod adapter;
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
pub mod debug_server;

// Always-available re-exports
pub use error::{GraphError, NodeError, PortTypeMismatchInfo};
pub use graph::edge::EdgeData;
pub use graph::metadata::GraphMetadata;
pub use graph::node::{Node, NodeId};
pub use graph::port::Port;
pub use graph::types::{PortType, Value};
pub use graph::{Graph, Subgraph, SubgraphDirective};
pub use graph::step_compiler::{
    compile_steps, is_structural_type, HANDLER_BRANCH, HANDLER_FORK, HANDLER_JOIN,
    HANDLER_LOOP_END, HANDLER_LOOP_START, HANDLER_MERGE,
};
pub use mermaid::{export_mermaid, load_mermaid, MermaidError, Span};

// Runtime-only re-exports
#[cfg(feature = "runtime")]
pub use adapter::{
    AdapterCapabilities, AdapterRegistry, AiAdapter, AiRequest, AiResponse, ClaudeCliAdapter,
    ConversationConfig, ConversationHistory, ConversationMessage, MessageRole, MockAdapter,
    TokenUsage,
    CONVERSATION_HISTORY_KEY,
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
    sync_handler, BackoffStrategy, CancellationToken, ConcurrencyLimits, EventBus,
    EventBusError, EventSubscriber, ExecutionContext, ExecutionError, ExecutionEvent,
    ExecutionResult, Executor, HandlerRegistry, LoopController, LoopIterator, LoopState,
    NodeHandler, NodeState, Outputs, RetryConfig, SteppedExecutor, TickResult,
    TopologicalExecutor,
};
#[cfg(feature = "runtime")]
pub use handlers::{
    CatchHandler, DelayHandler, ErrorTransformHandler, FallbackHandler, GateHandler,
    LlmCallHandler, LogHandler, MergeHandler, PassthroughHandler, RhaiHandler, RetryHandler,
    SplitHandler, StepBranchHandler, StepForkHandler, StepJoinHandler, StepLoopEndHandler,
    StepLoopStartHandler, TransformHandler,
};
#[cfg(feature = "runtime")]
pub use registry::NodeRegistry;
#[cfg(feature = "runtime")]
pub use template::{PromptTemplate, TemplateError};
