use crate::adapter::{AiAdapter, AiRequest, CacheControl};
use crate::error::NodeError;
use crate::execute::blackboard::Blackboard;
use crate::execute::{CancellationToken, ExecutionContext, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::graph::Graph;
use crate::template::PromptTemplate;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

/// Sentinel string a prompt template places at the cache boundary point.
///
/// When `config.cache_prefix: true` is set and a rendered prompt contains
/// exactly one occurrence of this marker, the handler splits the prompt into
/// `[prefix, suffix]` and builds `AiRequest::prompt_blocks` with the prefix
/// marked `CacheControl::Ephemeral`. The marker itself is stripped.
///
/// The marker is chosen to not collide with psflow's `PromptTemplate`
/// escape syntax (`{{` → `{`): angle-bracket delimiters survive rendering
/// untouched.
///
/// See `20260418-171453-psflow-anthropic-adapter-design.md` §2.3.
pub const CACHE_BOUNDARY_SENTINEL: &str = "<<<cache_boundary>>>";

/// Node handler that delegates to an AI adapter via a prompt template.
///
/// Supports two modes:
/// - **Transform:** data flows in, LLM processes it, structured data flows out.
///   The response text (or structured JSON) is placed in the `"response"` output port.
/// - **Oracle:** the LLM makes a routing decision. The response is placed in
///   `"decision"` output port for downstream branch guards to evaluate.
///
/// Configuration (from node's `config` JSON):
/// - `prompt` (required): Template string with `{inputs.*}` / `{ctx.*}` placeholders.
/// - `adapter`: Adapter name override (uses default if absent).
/// - `model`: Model override passed to the adapter.
/// - `temperature`: Sampling temperature.
/// - `max_tokens`: Maximum tokens in response.
/// - `output_format`: `"text"` (default) or `"json"`.
/// - `mode`: `"transform"` (default) or `"oracle"`.
/// - `context_max_tokens`: Token budget for conversation history.
/// - `context_depth`: Max ancestor LLM exchanges to include.
pub struct LlmCallHandler {
    adapter: Arc<dyn AiAdapter>,
    /// Shared execution context for blackboard access.
    /// Set when running within an executor; None for standalone/test use.
    exec_ctx: Option<Arc<ExecutionContext>>,
    /// Graph reference for ancestor-scoped conversation history.
    /// When set, history is filtered to only include messages from ancestor nodes.
    graph: Arc<RwLock<Option<Arc<Graph>>>>,
}

impl LlmCallHandler {
    pub fn new(adapter: Arc<dyn AiAdapter>) -> Self {
        Self {
            adapter,
            exec_ctx: None,
            graph: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a handler with access to the execution context's blackboard.
    pub fn with_context(adapter: Arc<dyn AiAdapter>, ctx: Arc<ExecutionContext>) -> Self {
        Self {
            adapter,
            exec_ctx: Some(ctx),
            graph: Arc::new(RwLock::new(None)),
        }
    }

    /// Set the graph for ancestor-scoped conversation history filtering.
    /// When set, each LLM node only sees history from its ancestor nodes,
    /// not from parallel branches.
    pub fn set_graph(&self, graph: Arc<Graph>) {
        *self.graph.write().unwrap_or_else(|e| e.into_inner()) = Some(graph);
    }
}

impl NodeHandler for LlmCallHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let adapter = self.adapter.clone();
        let config = node.config.clone();
        let node_id = node.id.0.clone();
        let exec_ctx = self.exec_ctx.clone();
        let graph = self.graph.read().unwrap_or_else(|e| e.into_inner()).clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before LLM call".into(),
                });
            }

            // Extract prompt template from config
            let prompt_str = config
                .get("prompt")
                .and_then(|v| v.as_str())
                .ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': missing config.prompt"),
                    recoverable: false,
                })?;

            // Compile and render the prompt template
            let template = PromptTemplate::compile(prompt_str).map_err(|e| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': template error: {e}"),
                recoverable: false,
            })?;

            // Use the execution context's blackboard if available, otherwise empty
            let empty_bb = Blackboard::new();
            let rendered = if let Some(ref ctx) = exec_ctx {
                let bb = ctx.blackboard();
                template.render(&inputs, &bb)
            } else {
                template.render(&inputs, &empty_bb)
            }
            .map_err(|e| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': template render error: {e}"),
                recoverable: false,
            })?;

            // Assemble conversation history from blackboard (if available)
            let conversation_messages = if let Some(ref ctx) = exec_ctx {
                use crate::adapter::conversation::{
                    ConversationConfig, ConversationHistory, CONVERSATION_HISTORY_KEY,
                };
                use crate::execute::blackboard::BlackboardScope;

                let bb = ctx.blackboard();
                let mut history = bb
                    .get(CONVERSATION_HISTORY_KEY, &BlackboardScope::Global)
                    .and_then(ConversationHistory::from_value)
                    .unwrap_or_default();
                drop(bb);

                // Filter to ancestor path if graph is available.
                // This ensures parallel branches don't see each other's history.
                if let Some(ref g) = graph {
                    let ancestors = g.ancestors(&node_id.as_str().into());
                    history.messages.retain(|msg| {
                        msg.node_id
                            .as_ref()
                            .map(|id| ancestors.contains(&id.as_str().into()))
                            .unwrap_or(true) // Keep messages without node_id (e.g. system)
                    });
                }

                // Apply limits from config if specified
                let conv_config = ConversationConfig {
                    max_tokens: config
                        .get("context_max_tokens")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as usize),
                    max_depth: config
                        .get("context_depth")
                        .and_then(|v| v.as_u64())
                        .map(|v| v as usize),
                };
                history.apply_limits(&conv_config);

                history.messages
            } else {
                Vec::new()
            };

            // Build the AI request. If `config.cache_prefix: true` is set and
            // the rendered prompt contains the `{{cache_boundary}}` sentinel,
            // split the prompt into a cached prefix + uncached suffix and
            // populate `prompt_blocks`. Otherwise fall back to flat prompt.
            let cache_prefix = config
                .get("cache_prefix")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let mut req = if cache_prefix {
                if let Some((prefix, suffix)) = rendered.split_once(CACHE_BOUNDARY_SENTINEL) {
                    AiRequest::new(String::new())
                        .with_cached_prefix(prefix.to_owned(), suffix.to_owned())
                } else {
                    // No sentinel — no-op; ship the flat prompt as-is.
                    AiRequest::new(rendered)
                }
            } else {
                AiRequest::new(rendered)
            };

            req.conversation_history = conversation_messages;

            // `cache_conversation: true` marks the last message of the
            // conversation history as an ephemeral cache breakpoint.
            if config
                .get("cache_conversation")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                req.conversation_cache_control = CacheControl::Ephemeral;
            }

            if let Some(temp) = config.get("temperature").and_then(|v| v.as_f64()) {
                req.temperature = Some(temp as f32);
            }
            if let Some(tokens) = config.get("max_tokens").and_then(|v| v.as_u64()) {
                req.max_tokens = Some(tokens as usize);
            }
            if let Some(model) = config.get("model").and_then(|v| v.as_str()) {
                req.model = Some(model.to_string());
            }

            let output_format = config
                .get("output_format")
                .and_then(|v| v.as_str())
                .unwrap_or("text");

            if output_format == "json" {
                req.output_schema = Some(serde_json::json!({"type": "object"}));
            }

            // Check cancellation before the potentially long LLM call
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before LLM call".into(),
                });
            }

            // Save prompt text for conversation history before the call
            let prompt_text = req.prompt.clone();

            // Make the adapter call
            let response = adapter.complete(req).await?;

            // Build outputs
            let mut outputs = Outputs::new();
            let mode = config
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("transform");

            let output_key = if mode == "oracle" {
                "decision"
            } else {
                "response"
            };

            // Capture response text for conversation history before moving fields
            let response_text_for_history = response
                .structured
                .as_ref()
                .map(|s| serde_json::to_string(s).unwrap_or_else(|_| response.text.clone()))
                .unwrap_or_else(|| response.text.clone());

            // If structured output is available, use it; otherwise use text
            if let Some(structured) = response.structured {
                outputs.insert(
                    output_key.into(),
                    Value::Domain {
                        type_name: "json".into(),
                        data: structured,
                    },
                );
            } else {
                outputs.insert(output_key.into(), Value::String(response.text));
            }

            // Accumulate this exchange into conversation history on the blackboard
            if let Some(ref ctx) = exec_ctx {
                use crate::adapter::conversation::{ConversationHistory, CONVERSATION_HISTORY_KEY};
                use crate::execute::blackboard::BlackboardScope;

                let mut bb = ctx.blackboard();
                let mut history = bb
                    .get(CONVERSATION_HISTORY_KEY, &BlackboardScope::Global)
                    .and_then(ConversationHistory::from_value)
                    .unwrap_or_default();
                history.push_exchange(&node_id, &prompt_text, &response_text_for_history);
                bb.set(
                    CONVERSATION_HISTORY_KEY.into(),
                    history.to_value(),
                    BlackboardScope::Global,
                );
            }

            // Always include usage metadata
            outputs.insert(
                "_usage_input_tokens".into(),
                Value::I64(response.usage.input_tokens as i64),
            );
            outputs.insert(
                "_usage_output_tokens".into(),
                Value::I64(response.usage.output_tokens as i64),
            );
            if let Some(read) = response.usage.cache_read_input_tokens {
                outputs.insert(
                    "_usage_cache_read_input_tokens".into(),
                    Value::I64(read as i64),
                );
            }
            if let Some(created) = response.usage.cache_creation_input_tokens {
                outputs.insert(
                    "_usage_cache_creation_input_tokens".into(),
                    Value::I64(created as i64),
                );
            }
            outputs.insert("_latency_ms".into(), Value::I64(response.latency_ms as i64));

            Ok(outputs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::mock::MockAdapter;
    use crate::graph::node::Node;

    #[tokio::test]
    async fn transform_mode_returns_response() {
        let adapter = Arc::new(MockAdapter::new().with_response("summarize", "A brief summary."));
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM1", "Summarize");
        node.config = serde_json::json!({
            "prompt": "Please summarize: {inputs.text}"
        });

        let mut inputs = Outputs::new();
        inputs.insert("text".into(), Value::String("Long article...".into()));

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            result.get("response"),
            Some(&Value::String("A brief summary.".into()))
        );
        assert!(result.contains_key("_latency_ms"));
    }

    #[tokio::test]
    async fn oracle_mode_returns_decision() {
        let adapter = Arc::new(MockAdapter::new().with_response("classify", "tech"));
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM2", "Classify");
        node.config = serde_json::json!({
            "prompt": "classify this: {inputs.text}",
            "mode": "oracle"
        });

        let mut inputs = Outputs::new();
        inputs.insert("text".into(), Value::String("AI news".into()));

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.get("decision"), Some(&Value::String("tech".into())));
    }

    #[tokio::test]
    async fn json_output_format() {
        let adapter = Arc::new(MockAdapter::new().with_response("analyze", r#"{"score": 0.9}"#));
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM3", "Analyze");
        node.config = serde_json::json!({
            "prompt": "analyze: {inputs.data}",
            "output_format": "json"
        });

        let mut inputs = Outputs::new();
        inputs.insert("data".into(), Value::String("test data".into()));

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        match result.get("response") {
            Some(Value::Domain { type_name, data }) => {
                assert_eq!(type_name, "json");
                assert_eq!(data["score"], 0.9);
            }
            other => panic!("expected Domain(json), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_prompt_is_error() {
        let adapter = Arc::new(MockAdapter::new());
        let handler = LlmCallHandler::new(adapter);

        let node = Node::new("LLM4", "NoPrompt");
        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancellation_before_call() {
        let adapter = Arc::new(MockAdapter::new());
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM5", "Cancelled");
        node.config = serde_json::json!({ "prompt": "test" });

        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = handler.execute(&node, Outputs::new(), cancel).await;
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    #[tokio::test]
    async fn config_passes_through_to_request() {
        let adapter = Arc::new(MockAdapter::new().with_default("ok"));
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM6", "Configured");
        node.config = serde_json::json!({
            "prompt": "test",
            "temperature": 0.3,
            "max_tokens": 500,
            "model": "claude-sonnet"
        });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        // The mock adapter doesn't validate these, but the handler should
        // produce a valid response
        assert!(result.contains_key("response"));
    }

    // -- Conversation history accumulation tests --

    #[tokio::test]
    async fn conversation_history_accumulates_on_blackboard() {
        use crate::adapter::conversation::{ConversationHistory, CONVERSATION_HISTORY_KEY};
        use crate::execute::blackboard::BlackboardScope;

        let adapter = Arc::new(MockAdapter::new().with_default("response text"));
        let ctx = Arc::new(ExecutionContext::new());
        let handler = LlmCallHandler::with_context(adapter, ctx.clone());

        // First LLM call
        let mut node1 = Node::new("LLM_A", "First");
        node1.config = serde_json::json!({ "prompt": "Hello" });
        handler
            .execute(&node1, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        // Check history has 1 exchange (2 messages)
        let history = {
            let bb = ctx.blackboard();
            bb.get(CONVERSATION_HISTORY_KEY, &BlackboardScope::Global)
                .and_then(ConversationHistory::from_value)
                .unwrap()
        };
        assert_eq!(history.len(), 2);
        assert_eq!(
            history.messages[0].role,
            crate::adapter::conversation::MessageRole::User
        );
        assert_eq!(history.messages[0].node_id, Some("LLM_A".into()));
        assert_eq!(
            history.messages[1].role,
            crate::adapter::conversation::MessageRole::Assistant
        );

        // Second LLM call
        let mut node2 = Node::new("LLM_B", "Second");
        node2.config = serde_json::json!({ "prompt": "Follow up" });
        handler
            .execute(&node2, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        // History now has 2 exchanges (4 messages)
        let bb = ctx.blackboard();
        let history = bb
            .get(CONVERSATION_HISTORY_KEY, &BlackboardScope::Global)
            .and_then(ConversationHistory::from_value)
            .unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(history.messages[2].node_id, Some("LLM_B".into()));
    }

    #[tokio::test]
    async fn conversation_history_passed_in_request() {
        use crate::adapter::conversation::{
            ConversationHistory, ConversationMessage, CONVERSATION_HISTORY_KEY,
        };
        use crate::execute::blackboard::BlackboardScope;

        // Pre-populate history on the blackboard
        let ctx = Arc::new(ExecutionContext::new());
        {
            let mut history = ConversationHistory::new();
            history.push(ConversationMessage::user("Prior question").with_node("OLD"));
            history.push(ConversationMessage::assistant("Prior answer").with_node("OLD"));

            let mut bb = ctx.blackboard();
            bb.set(
                CONVERSATION_HISTORY_KEY.into(),
                history.to_value(),
                BlackboardScope::Global,
            );
        }

        // The mock adapter just returns "ok" — we verify the handler assembled
        // the request with conversation_history by checking the blackboard after
        let adapter = Arc::new(MockAdapter::new().with_default("new answer"));
        let handler = LlmCallHandler::with_context(adapter, ctx.clone());

        let mut node = Node::new("LLM_NEW", "New call");
        node.config = serde_json::json!({ "prompt": "New question" });
        handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        // History should now have 2 old + 2 new = 4 messages
        let bb = ctx.blackboard();
        let history = bb
            .get(CONVERSATION_HISTORY_KEY, &BlackboardScope::Global)
            .and_then(ConversationHistory::from_value)
            .unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(history.messages[0].content, "Prior question");
        assert_eq!(history.messages[2].content, "New question");
        assert_eq!(history.messages[3].content, "new answer");
    }

    #[tokio::test]
    async fn no_history_without_context() {
        // Without ExecutionContext, no history accumulation (standalone use)
        let adapter = Arc::new(MockAdapter::new().with_default("ok"));
        let handler = LlmCallHandler::new(adapter); // No context

        let mut node = Node::new("LLM", "Standalone");
        node.config = serde_json::json!({ "prompt": "test" });
        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        // Should still work, just no history side effect
        assert!(result.contains_key("response"));
    }

    #[tokio::test]
    async fn ancestor_scoped_history_excludes_parallel_branches() {
        use crate::adapter::conversation::{
            ConversationHistory, ConversationMessage, MessageRole, CONVERSATION_HISTORY_KEY,
        };
        use crate::adapter::{AdapterCapabilities, AiAdapter, AiRequest, AiResponse, TokenUsage};
        use crate::execute::blackboard::BlackboardScope;
        use std::sync::Mutex;

        // Recording adapter that captures conversation_history from each request
        struct RecordingAdapter {
            captured: Mutex<Vec<Vec<ConversationMessage>>>,
        }
        impl RecordingAdapter {
            fn new() -> Self {
                Self {
                    captured: Mutex::new(Vec::new()),
                }
            }
            fn captured_histories(&self) -> Vec<Vec<ConversationMessage>> {
                self.captured.lock().unwrap().clone()
            }
        }
        impl AiAdapter for RecordingAdapter {
            fn complete(
                &self,
                req: AiRequest,
            ) -> Pin<Box<dyn Future<Output = Result<AiResponse, NodeError>> + Send + '_>>
            {
                self.captured.lock().unwrap().push(req.conversation_history);
                Box::pin(async {
                    Ok(AiResponse {
                        text: "recorded".into(),
                        structured: None,
                        usage: TokenUsage::default(),
                        latency_ms: 0,
                    })
                })
            }
            fn judge(
                &self,
                _candidates: &[String],
                _criteria: &str,
            ) -> Pin<Box<dyn Future<Output = Result<usize, NodeError>> + Send + '_>> {
                Box::pin(async { Ok(0) })
            }
            fn capabilities(&self) -> &AdapterCapabilities {
                &AdapterCapabilities {
                    tool_use: false,
                    structured_output: false,
                    vision: false,
                    conversation_history: true,
                    max_tokens: None,
                }
            }
            fn name(&self) -> &str {
                "recording"
            }
        }

        // Graph: A → B, A → C (B and C are parallel)
        let mut graph = Graph::new();
        graph.add_node(Node::new("A", "Root")).unwrap();
        graph.add_node(Node::new("B", "Branch B")).unwrap();
        graph.add_node(Node::new("C", "Branch C")).unwrap();
        graph
            .add_edge(&"A".into(), "out", &"B".into(), "in", None)
            .unwrap();
        graph
            .add_edge(&"A".into(), "out", &"C".into(), "in", None)
            .unwrap();

        let ctx = Arc::new(ExecutionContext::new());

        // Pre-populate: A and B have already run
        {
            let mut history = ConversationHistory::new();
            history.push_exchange("A", "Root prompt", "Root response");
            history.push_exchange("B", "Branch B prompt", "Branch B response");

            let mut bb = ctx.blackboard();
            bb.set(
                CONVERSATION_HISTORY_KEY.into(),
                history.to_value(),
                BlackboardScope::Global,
            );
        }

        // Run node C with recording adapter
        let adapter = Arc::new(RecordingAdapter::new());
        let handler = LlmCallHandler::with_context(adapter.clone(), ctx.clone());
        handler.set_graph(Arc::new(graph));

        let mut node_c = Node::new("C", "Branch C");
        node_c.config = serde_json::json!({ "prompt": "Branch C prompt" });
        handler
            .execute(&node_c, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        // Verify the actual request sent to the adapter
        let histories = adapter.captured_histories();
        assert_eq!(histories.len(), 1);
        let sent_history = &histories[0];

        // C's ancestors are {A} — only A's messages should be in the request
        assert_eq!(sent_history.len(), 2); // 1 exchange = 2 messages
        assert_eq!(sent_history[0].node_id, Some("A".into()));
        assert_eq!(sent_history[0].role, MessageRole::User);
        assert_eq!(sent_history[0].content, "Root prompt");
        assert_eq!(sent_history[1].node_id, Some("A".into()));
        assert_eq!(sent_history[1].role, MessageRole::Assistant);

        // B's messages must NOT be present
        assert!(
            !sent_history
                .iter()
                .any(|m| m.node_id.as_deref() == Some("B")),
            "parallel branch B's messages should be excluded"
        );
    }

    // -- Prompt cache wiring tests (§5–§7 prerequisite) --
    //
    // See `20260418-171453-psflow-anthropic-adapter-design.md` §2.3.

    /// Adapter that captures the last `AiRequest` it received. Used to assert
    /// the handler's cache-config → `AiRequest` translation without relying on
    /// a live HTTP adapter.
    struct CapturingAdapter {
        captured: std::sync::Mutex<Option<AiRequest>>,
        cache_usage: (Option<usize>, Option<usize>),
    }

    impl CapturingAdapter {
        fn new() -> Self {
            Self {
                captured: std::sync::Mutex::new(None),
                cache_usage: (None, None),
            }
        }
        fn with_cache_usage(
            mut self,
            cache_read: Option<usize>,
            cache_creation: Option<usize>,
        ) -> Self {
            self.cache_usage = (cache_read, cache_creation);
            self
        }
        fn taken(&self) -> AiRequest {
            self.captured
                .lock()
                .unwrap()
                .take()
                .expect("request captured")
        }
    }

    impl crate::adapter::AiAdapter for CapturingAdapter {
        fn complete(
            &self,
            req: AiRequest,
        ) -> Pin<Box<dyn Future<Output = Result<crate::adapter::AiResponse, NodeError>> + Send + '_>>
        {
            *self.captured.lock().unwrap() = Some(req);
            let (cache_read, cache_creation) = self.cache_usage;
            Box::pin(async move {
                Ok(crate::adapter::AiResponse {
                    text: "captured".into(),
                    structured: None,
                    usage: crate::adapter::TokenUsage {
                        input_tokens: 10,
                        output_tokens: 20,
                        cache_read_input_tokens: cache_read,
                        cache_creation_input_tokens: cache_creation,
                    },
                    latency_ms: 0,
                })
            })
        }
        fn judge(
            &self,
            _candidates: &[String],
            _criteria: &str,
        ) -> Pin<Box<dyn Future<Output = Result<usize, NodeError>> + Send + '_>> {
            Box::pin(async { Ok(0) })
        }
        fn capabilities(&self) -> &crate::adapter::AdapterCapabilities {
            use std::sync::OnceLock;
            static CAPS: OnceLock<crate::adapter::AdapterCapabilities> = OnceLock::new();
            CAPS.get_or_init(|| crate::adapter::AdapterCapabilities {
                tool_use: false,
                structured_output: false,
                vision: false,
                conversation_history: true,
                max_tokens: None,
            })
        }
        fn name(&self) -> &str {
            "capturing"
        }
    }

    #[tokio::test]
    async fn cache_prefix_false_leaves_flat_prompt() {
        let adapter = Arc::new(CapturingAdapter::new());
        let handler = LlmCallHandler::new(adapter.clone());

        let mut node = Node::new("LLM_CP_OFF", "No cache");
        node.config = serde_json::json!({
            "prompt": "PREFIX <<<cache_boundary>>> SUFFIX",
            "cache_prefix": false,
        });

        handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        let req = adapter.taken();
        assert!(
            req.prompt_blocks.is_none(),
            "cache_prefix:false must not produce prompt_blocks"
        );
        // Sentinel is not stripped — it's just a literal string when caching is off.
        assert!(req.prompt.contains(CACHE_BOUNDARY_SENTINEL));
    }

    #[tokio::test]
    async fn cache_prefix_true_with_sentinel_splits_into_blocks() {
        let adapter = Arc::new(CapturingAdapter::new());
        let handler = LlmCallHandler::new(adapter.clone());

        let mut node = Node::new("LLM_CP_ON", "Cache on");
        node.config = serde_json::json!({
            "prompt": "CACHED_PREFIX_TEXT<<<cache_boundary>>>UNCACHED_SUFFIX_TEXT",
            "cache_prefix": true,
        });

        handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        let req = adapter.taken();
        let blocks = req.prompt_blocks.expect("prompt_blocks populated");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "CACHED_PREFIX_TEXT");
        assert_eq!(blocks[0].cache_control, CacheControl::Ephemeral);
        assert_eq!(blocks[1].text, "UNCACHED_SUFFIX_TEXT");
        assert_eq!(blocks[1].cache_control, CacheControl::None);
        // The flat prompt is the concatenation for fallback adapters.
        assert_eq!(req.prompt, "CACHED_PREFIX_TEXTUNCACHED_SUFFIX_TEXT");
    }

    #[tokio::test]
    async fn cache_prefix_true_without_sentinel_is_noop() {
        let adapter = Arc::new(CapturingAdapter::new());
        let handler = LlmCallHandler::new(adapter.clone());

        let mut node = Node::new("LLM_CP_NOSENTINEL", "Missing sentinel");
        node.config = serde_json::json!({
            "prompt": "no sentinel here",
            "cache_prefix": true,
        });

        handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        let req = adapter.taken();
        assert!(
            req.prompt_blocks.is_none(),
            "cache_prefix:true without sentinel must not populate prompt_blocks"
        );
        assert_eq!(req.prompt, "no sentinel here");
    }

    #[tokio::test]
    async fn cache_conversation_false_leaves_control_none() {
        let adapter = Arc::new(CapturingAdapter::new());
        let handler = LlmCallHandler::new(adapter.clone());

        let mut node = Node::new("LLM_CC_OFF", "No conv cache");
        node.config = serde_json::json!({
            "prompt": "hi",
            "cache_conversation": false,
        });

        handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        let req = adapter.taken();
        assert_eq!(req.conversation_cache_control, CacheControl::None);
    }

    #[tokio::test]
    async fn cache_conversation_true_marks_ephemeral() {
        let adapter = Arc::new(CapturingAdapter::new());
        let handler = LlmCallHandler::new(adapter.clone());

        let mut node = Node::new("LLM_CC_ON", "Conv cache");
        node.config = serde_json::json!({
            "prompt": "hi",
            "cache_conversation": true,
        });

        handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        let req = adapter.taken();
        assert_eq!(req.conversation_cache_control, CacheControl::Ephemeral);
    }

    #[tokio::test]
    async fn cache_metrics_surface_on_outputs_when_reported() {
        // Adapter reports cache_read=5000 / cache_creation=0 — the handler
        // must surface both as `_usage_cache_*_input_tokens` outputs.
        let adapter = Arc::new(CapturingAdapter::new().with_cache_usage(Some(5000), Some(0)));
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM_METRIC", "With cache metrics");
        node.config = serde_json::json!({ "prompt": "x" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            result.get("_usage_cache_read_input_tokens"),
            Some(&Value::I64(5000))
        );
        assert_eq!(
            result.get("_usage_cache_creation_input_tokens"),
            Some(&Value::I64(0))
        );
    }

    #[tokio::test]
    async fn cache_metrics_absent_when_adapter_does_not_report() {
        // Default mock/capturing adapter reports None for cache fields — the
        // handler must omit the cache output keys entirely.
        let adapter = Arc::new(CapturingAdapter::new());
        let handler = LlmCallHandler::new(adapter);

        let mut node = Node::new("LLM_NOMETRIC", "No cache metrics");
        node.config = serde_json::json!({ "prompt": "x" });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        assert!(
            !result.contains_key("_usage_cache_read_input_tokens"),
            "cache read key must be absent when adapter reports None"
        );
        assert!(
            !result.contains_key("_usage_cache_creation_input_tokens"),
            "cache creation key must be absent when adapter reports None"
        );
    }
}
