//! Graph-load validation pass.
//!
//! Handlers implement [`NodeHandler::validate_node`] to report load-time
//! misconfiguration (shape errors, missing references, script compile
//! errors). [`validate_graph`] walks every node in a graph, dispatches to
//! the appropriate handler's `validate_node`, and aggregates every issue
//! into a single [`ValidationReport`] — so a misconfigured graph surfaces
//! all problems at once rather than failing one at a time.
//!
//! Executors call this pass after [`super::auto_install_auth_registry`]
//! and before emitting `ExecutionEvent::ExecutionStarted`.

use super::{ExecutionContext, ExecutionError, HandlerRegistry};
use crate::graph::Graph;
use serde::{Deserialize, Serialize};

/// Category of graph-load validation failure. Free-form enough to cover
/// what the current handlers surface without prescribing a fixed taxonomy
/// for future additions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationIssueKind {
    /// Config shape is wrong (missing key, wrong type, unparsable value).
    Config,
    /// Config references a resource that isn't defined (subgraph name,
    /// strategy name, handler name, etc.).
    MissingReference,
    /// An embedded script (Rhai predicate, etc.) failed to compile.
    ScriptCompile,
    /// Two otherwise-valid config keys are mutually exclusive.
    Incompatibility,
}

/// A single graph-load validation failure tied to a specific node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationIssue {
    /// The node whose config surfaced the issue.
    pub node_id: String,
    /// The handler name the node resolves to (for operator diagnosis).
    pub handler: String,
    /// Category — see [`ValidationIssueKind`].
    pub kind: ValidationIssueKind,
    /// Human-readable message. Already includes enough context for the
    /// operator to act without cross-referencing the kind.
    pub message: String,
}

impl ValidationIssue {
    pub fn new(
        node_id: impl Into<String>,
        handler: impl Into<String>,
        kind: ValidationIssueKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            node_id: node_id.into(),
            handler: handler.into(),
            kind,
            message: message.into(),
        }
    }
}

/// Aggregated report from [`validate_graph`]. Empty issues ⇒ graph is
/// valid as far as the installed handlers can tell.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ValidationReport {
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub fn is_empty(&self) -> bool {
        self.issues.is_empty()
    }

    pub fn into_error(self) -> ExecutionError {
        ExecutionError::ValidationFailed(self.render_message())
    }

    /// Render the report as a single human-readable multi-line string.
    /// Used as the payload for [`ExecutionError::ValidationFailed`] so
    /// embedders see every issue without having to destructure the report.
    pub fn render_message(&self) -> String {
        if self.issues.is_empty() {
            return "no issues".to_string();
        }
        let mut s = format!("{} graph validation issue(s):", self.issues.len());
        for issue in &self.issues {
            s.push_str(&format!(
                "\n  - node '{}' ({}): {}",
                issue.node_id, issue.handler, issue.message
            ));
        }
        s
    }
}

/// Walk every node in `graph`, resolve its handler, and call
/// [`super::NodeHandler::validate_node`] to collect any load-time issues.
///
/// Returns `Ok(())` iff every handler reports clean. Otherwise returns
/// `Err(ExecutionError::ValidationFailed)` carrying the full issue list
/// rendered as a multi-line message. Nodes without a declared handler,
/// and nodes whose declared handler is not registered, are skipped — the
/// unregistered-handler case is covered by the executor's own missing-
/// handler guard and is not re-reported here.
pub fn validate_graph(
    graph: &Graph,
    handlers: &HandlerRegistry,
    ctx: &ExecutionContext,
) -> Result<(), ExecutionError> {
    let mut report = ValidationReport::default();
    for node in graph.nodes() {
        let Some(handler_name) = node.handler.as_deref() else {
            continue;
        };
        let Some(handler) = handlers.get(handler_name) else {
            continue;
        };
        if let Err(mut issues) = handler.validate_node(node, graph, ctx) {
            // Fill in handler name on any issue the handler didn't
            // populate itself — keeps per-handler impls concise without
            // leaking that convention into the trait shape.
            for i in issues.iter_mut() {
                if i.handler.is_empty() {
                    i.handler = handler_name.to_string();
                }
                if i.node_id.is_empty() {
                    i.node_id = node.id.0.clone();
                }
            }
            report.issues.append(&mut issues);
        }
    }
    if report.is_empty() {
        Ok(())
    } else {
        Err(report.into_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::{CancellationToken, NodeHandler, Outputs};
    use crate::graph::node::Node;
    use crate::graph::Graph;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    struct OkHandler;
    impl NodeHandler for OkHandler {
        fn execute(
            &self,
            _node: &Node,
            inputs: Outputs,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<Outputs, crate::error::NodeError>> + Send>>
        {
            Box::pin(async move { Ok(inputs) })
        }
    }

    struct OneIssueHandler;
    impl NodeHandler for OneIssueHandler {
        fn execute(
            &self,
            _node: &Node,
            inputs: Outputs,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<Outputs, crate::error::NodeError>> + Send>>
        {
            Box::pin(async move { Ok(inputs) })
        }
        fn validate_node(
            &self,
            _node: &Node,
            _graph: &Graph,
            _ctx: &ExecutionContext,
        ) -> Result<(), Vec<ValidationIssue>> {
            Err(vec![ValidationIssue::new(
                "",
                "",
                ValidationIssueKind::Config,
                "one issue",
            )])
        }
    }

    struct TwoIssuesHandler;
    impl NodeHandler for TwoIssuesHandler {
        fn execute(
            &self,
            _node: &Node,
            inputs: Outputs,
            _cancel: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = Result<Outputs, crate::error::NodeError>> + Send>>
        {
            Box::pin(async move { Ok(inputs) })
        }
        fn validate_node(
            &self,
            _node: &Node,
            _graph: &Graph,
            _ctx: &ExecutionContext,
        ) -> Result<(), Vec<ValidationIssue>> {
            Err(vec![
                ValidationIssue::new("", "", ValidationIssueKind::Config, "a"),
                ValidationIssue::new("", "", ValidationIssueKind::ScriptCompile, "b"),
            ])
        }
    }

    #[test]
    fn empty_graph_is_valid() {
        let graph = Graph::new();
        let handlers: HandlerRegistry = Default::default();
        let ctx = ExecutionContext::new();
        assert!(validate_graph(&graph, &handlers, &ctx).is_ok());
    }

    #[test]
    fn all_clean_handlers_pass() {
        let mut graph = Graph::new();
        graph
            .add_node(Node::new("A", "A").with_handler("ok"))
            .unwrap();
        graph
            .add_node(Node::new("B", "B").with_handler("ok"))
            .unwrap();
        let mut handlers: HandlerRegistry = Default::default();
        handlers.insert("ok".into(), Arc::new(OkHandler));
        let ctx = ExecutionContext::new();
        assert!(validate_graph(&graph, &handlers, &ctx).is_ok());
    }

    #[test]
    fn single_issue_surfaces() {
        let mut graph = Graph::new();
        graph
            .add_node(Node::new("A", "A").with_handler("bad"))
            .unwrap();
        let mut handlers: HandlerRegistry = Default::default();
        handlers.insert("bad".into(), Arc::new(OneIssueHandler));
        let ctx = ExecutionContext::new();
        let err = validate_graph(&graph, &handlers, &ctx).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("one issue"));
        assert!(msg.contains("node 'A'"));
        assert!(msg.contains("bad"));
    }

    #[test]
    fn many_issues_aggregate() {
        let mut graph = Graph::new();
        graph
            .add_node(Node::new("A", "A").with_handler("bad2"))
            .unwrap();
        graph
            .add_node(Node::new("B", "B").with_handler("bad"))
            .unwrap();
        graph
            .add_node(Node::new("C", "C").with_handler("bad2"))
            .unwrap();
        let mut handlers: HandlerRegistry = Default::default();
        handlers.insert("bad".into(), Arc::new(OneIssueHandler));
        handlers.insert("bad2".into(), Arc::new(TwoIssuesHandler));
        let ctx = ExecutionContext::new();
        let err = validate_graph(&graph, &handlers, &ctx).unwrap_err();
        let msg = err.to_string();
        // 2 + 1 + 2 = 5 issues total. Rendering uses "5 graph validation issue(s)".
        assert!(msg.contains("5 graph validation"));
    }

    #[test]
    fn missing_handler_is_skipped() {
        // Unregistered handler names are the executor's problem
        // (HandlerNotFound), not this pass's.
        let mut graph = Graph::new();
        graph
            .add_node(Node::new("A", "A").with_handler("nope"))
            .unwrap();
        let handlers: HandlerRegistry = Default::default();
        let ctx = ExecutionContext::new();
        assert!(validate_graph(&graph, &handlers, &ctx).is_ok());
    }

    #[test]
    fn report_render_message_shape() {
        let r = ValidationReport {
            issues: vec![
                ValidationIssue::new("X", "h1", ValidationIssueKind::Config, "bad config"),
                ValidationIssue::new("Y", "h2", ValidationIssueKind::ScriptCompile, "bad script"),
            ],
        };
        let m = r.render_message();
        assert!(m.contains("2 graph validation issue(s)"));
        assert!(m.contains("node 'X' (h1): bad config"));
        assert!(m.contains("node 'Y' (h2): bad script"));
    }
}
