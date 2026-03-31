pub mod blackboard;
pub mod concurrency;
pub mod context;
pub mod control;
pub mod event;
pub mod event_bus;
pub mod event_driven;
pub mod lifecycle;
pub mod reactive;
pub mod retry;
pub mod stepped;
pub mod topological;
pub mod trace;

pub use blackboard::{Blackboard, BlackboardScope, ContextInheritance};
pub use concurrency::ConcurrencyLimits;
pub use context::{CancellationToken, ExecutionContext};
pub use event::ExecutionEvent;
pub use event_bus::{EventBus, EventBusError, EventSubscriber};
pub use lifecycle::NodeState;
pub use retry::{BackoffStrategy, RetryConfig};
pub use event_driven::{EventDrivenExecutor, EventMessage, EventSender};
pub use reactive::ReactiveExecutor;
pub use stepped::{SteppedExecutor, TickResult};
pub use topological::TopologicalExecutor;

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
