pub mod blackboard;
pub mod concurrency;
pub mod context;
pub mod control;
pub mod event;
pub mod event_bus;
pub mod event_driven;
pub mod lifecycle;
pub mod loop_controller;
pub mod reactive;
pub mod retry;
pub mod snapshot;
pub mod stepped;
pub mod topological;
pub mod trace;
pub mod validation;

pub use blackboard::{Blackboard, BlackboardScope, ContextInheritance};
pub use concurrency::ConcurrencyLimits;
pub use context::{CancellationToken, ExecutionContext};
pub use event::ExecutionEvent;
pub use event_bus::{EventBus, EventBusError, EventSubscriber};
pub use event_driven::{EventDrivenExecutor, EventMessage, EventSender};
pub use lifecycle::NodeState;
pub use loop_controller::{LoopController, LoopIterator, LoopState};
pub use reactive::ReactiveExecutor;
pub use retry::{BackoffStrategy, RetryConfig};
pub use stepped::{SteppedExecutor, TickResult};
pub use topological::TopologicalExecutor;
pub use validation::{validate_graph, ValidationIssue, ValidationIssueKind, ValidationReport};

use crate::auth::AuthStrategyRegistry;
use crate::error::NodeError;
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::graph::Graph;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// Outputs produced by a node handler — named values keyed by port name.
pub type Outputs = HashMap<String, Value>;

/// Registry mapping handler names to implementations.
pub type HandlerRegistry = HashMap<String, Arc<dyn NodeHandler>>;

/// How an embedder should treat a handler's execution shape.
///
/// psflow defines just the built-in kinds it understands. Embedders with
/// additional runtime semantics (e.g. ergon-core's `Agentic` for MCP
/// suspend/yield) layer their own enum on top; no need to encode those
/// concepts upstream.
///
/// - `Deterministic` — the handler computes outputs inline and returns. No
///   external coordination required. This is the default.
/// - `Trigger` — the handler is an entry-node driven by external events
///   (timers, file watches, webhooks, etc.); executors treat it as a graph
///   source rather than a compute step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerKind {
    Deterministic,
    Trigger,
}

/// Self-describing metadata for a [`NodeHandler`].
///
/// Handlers implement [`NodeHandler::schema`] to surface their configuration
/// keys, expected inputs, and produced outputs so tooling (docs generators,
/// graph editors, validators) can reflect over the registry without running
/// the workflow.
///
/// The shape mirrors a minimal JSON-schema-flavoured description — every
/// property carries a name, a type-hint string, whether it is required, and
/// a short human-readable description. The simplicity is intentional: the
/// full JSON Schema spec is overkill for catalogue-style tooling, and we
/// serialise directly to a JSON object via serde.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HandlerSchema {
    /// Canonical handler name (registry key).
    pub name: String,
    /// Human-readable summary.
    pub description: String,
    /// Configuration keys read from `node.config` at execution time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config: Vec<SchemaField>,
    /// Named input ports the handler consumes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<SchemaField>,
    /// Named output ports the handler produces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<SchemaField>,
}

impl HandlerSchema {
    /// Build an opaque placeholder schema — only the name is known. Used as
    /// the default when a handler has not implemented `schema()` yet.
    pub fn opaque(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: "no schema provided".to_string(),
            config: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
        }
    }

    /// Start a new schema with the given name and description.
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            config: Vec::new(),
            inputs: Vec::new(),
            outputs: Vec::new(),
        }
    }

    pub fn with_config(mut self, field: SchemaField) -> Self {
        self.config.push(field);
        self
    }

    pub fn with_input(mut self, field: SchemaField) -> Self {
        self.inputs.push(field);
        self
    }

    pub fn with_output(mut self, field: SchemaField) -> Self {
        self.outputs.push(field);
        self
    }
}

/// A single named field within a [`HandlerSchema`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SchemaField {
    pub name: String,
    /// Type hint — free-form string (`"string"`, `"integer"`, `"map"`,
    /// `"string|array<string>"`, etc.). Not validated — consumed by tooling.
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Optional default value as JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Value>,
}

impl SchemaField {
    pub fn new(name: impl Into<String>, ty: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ty: ty.into(),
            required: false,
            description: String::new(),
            default: None,
        }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    pub fn default(mut self, value: serde_json::Value) -> Self {
        self.default = Some(value);
        self
    }
}

/// A node handler that processes inputs and produces outputs.
///
/// The `cancel` token enables cooperative cancellation — handlers should check
/// `cancel.is_cancelled()` at natural yield points and return `NodeError::Cancelled`
/// if set. Returns a `'static` future so it can be spawned on tokio.
pub trait NodeHandler: Send + Sync {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>>;

    /// Self-declare the handler's execution shape.
    ///
    /// Default: [`HandlerKind::Deterministic`]. Override on entry-node
    /// handlers that should be classified as triggers.
    fn kind(&self) -> HandlerKind {
        HandlerKind::Deterministic
    }

    /// Self-describe the handler's configuration, inputs, and outputs.
    ///
    /// Default: an opaque placeholder carrying only the registry name. Override
    /// on built-in handlers to expose schema metadata for tooling (catalogue
    /// generators, validators, graph editors).
    ///
    /// The `name` argument is the registry key this handler is registered
    /// under — handlers can be registered under multiple names, so the schema
    /// is parameterised by name rather than baked into the type.
    fn schema(&self, name: &str) -> HandlerSchema {
        HandlerSchema::opaque(name)
    }

    /// Graph-load validation hook.
    ///
    /// Called once per node during [`validation::validate_graph`] before any
    /// node runs. Handlers return [`ValidationIssue`] entries for every
    /// problem surfaced by their config (shape errors, missing references,
    /// script compile errors, mutually-exclusive keys, …) — the aggregator
    /// collects them across nodes so a misconfigured graph surfaces every
    /// issue at once rather than failing attempt-by-attempt.
    ///
    /// Default: return `Ok(())` — handlers that do not need load-time
    /// validation pay no cost. Handlers may leave `issue.node_id` and
    /// `issue.handler` unset; the aggregator fills them in from the node
    /// being walked.
    fn validate_node(
        &self,
        _node: &Node,
        _graph: &Graph,
        _ctx: &ExecutionContext,
    ) -> Result<(), Vec<validation::ValidationIssue>> {
        Ok(())
    }
}

/// Strategy for executing a graph. The future borrows self, graph, and handlers.
pub trait Executor: Send + Sync {
    fn execute<'a>(
        &'a self,
        graph: &'a Graph,
        handlers: &'a HandlerRegistry,
    ) -> Pin<Box<dyn Future<Output = Result<ExecutionResult, ExecutionError>> + Send + 'a>>;
}

/// Result of a complete graph execution.
#[derive(Debug)]
pub struct ExecutionResult {
    pub node_states: HashMap<String, NodeState>,
    pub node_outputs: HashMap<String, Outputs>,
    pub events: Vec<ExecutionEvent>,
    pub elapsed: std::time::Duration,
}

impl ExecutionResult {
    /// Build a structured execution trace from this result's events.
    pub fn trace(&self) -> trace::ExecutionTrace {
        trace::ExecutionTrace::from_events(&self.events)
    }
}

/// Errors from graph execution.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ExecutionError {
    #[error("validation failed: {0}")]
    ValidationFailed(String),
    #[error("handler not found for '{node_id}': '{handler}'")]
    HandlerNotFound { node_id: String, handler: String },
    #[error("cancelled")]
    Cancelled,
}

/// Auto-install an [`AuthStrategyRegistry`] on `ctx` from the graph's declared
/// auth strategies, if the graph has any and the embedder has not already
/// installed a registry.
///
/// **No-op conditions** (both are silent no-ops):
/// - `graph.metadata().auth` is empty — graph declares no auth strategies.
/// - `ctx.auth_registry()` is already `Some` — embedder pre-installed a
///   registry; that registry is authoritative and is left untouched.
///
/// **Error condition**: if a declared strategy references an unknown type or
/// has an invalid role binding, returns `ExecutionError::ValidationFailed`
/// before any node runs.
pub fn auto_install_auth_registry(
    graph: &Graph,
    ctx: &ExecutionContext,
) -> Result<(), ExecutionError> {
    let auth = &graph.metadata().auth;
    if auth.is_empty() {
        return Ok(());
    }
    // Embedder already installed — leave it alone.
    if ctx.auth_registry().is_some() {
        return Ok(());
    }
    let mut registry = AuthStrategyRegistry::with_builtins();
    registry
        .build_from_decls(auth)
        .map_err(|e| ExecutionError::ValidationFailed(e.to_string()))?;
    ctx.install_auth_registry(registry);
    Ok(())
}

/// Create a handler from a synchronous closure (cancel token ignored).
pub fn sync_handler<F>(f: F) -> Arc<dyn NodeHandler>
where
    F: Fn(&Node, Outputs) -> Result<Outputs, NodeError> + Send + Sync + 'static,
{
    Arc::new(SyncHandlerImpl(f))
}

struct SyncHandlerImpl<F>(F);

impl<F> NodeHandler for SyncHandlerImpl<F>
where
    F: Fn(&Node, Outputs) -> Result<Outputs, NodeError> + Send + Sync,
{
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        _cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let result = (self.0)(node, inputs);
        Box::pin(async move { result })
    }
}
