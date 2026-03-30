use crate::error::NodeError;
use crate::execute::{CancellationToken, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::graph::types::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// A prompt sent to the human operator, containing the data to review.
#[derive(Debug, Clone)]
pub struct HumanPrompt {
    /// The node ID requesting input.
    pub node_id: String,
    /// The node's label (human-readable description).
    pub label: String,
    /// The message to display (from `config.message`, or default).
    pub message: String,
    /// The data being presented for review (the node's inputs).
    pub data: Outputs,
}

/// Handle for the operator to respond to a human-in-the-loop prompt.
#[derive(Debug)]
pub struct HumanResponder {
    tx: tokio::sync::oneshot::Sender<Outputs>,
}

impl HumanResponder {
    /// Send a response back to the waiting node. The response becomes the node's outputs.
    pub fn respond(self, response: Outputs) {
        let _ = self.tx.send(response);
    }
}

/// Receiver side: the operator listens for prompts and sends responses.
///
/// ```ignore
/// let (handler, mut receiver) = HumanInputHandler::new();
/// // In operator task:
/// while let Some((prompt, responder)) = receiver.recv().await {
///     println!("Node {} asks: {}", prompt.node_id, prompt.message);
///     let mut response = Outputs::new();
///     response.insert("answer".into(), Value::String("approved".into()));
///     responder.respond(response);
/// }
/// ```
pub struct HumanInputReceiver {
    rx: mpsc::Receiver<(HumanPrompt, HumanResponder)>,
}

impl HumanInputReceiver {
    /// Wait for the next human-in-the-loop prompt.
    /// Returns `None` when all handlers are dropped.
    pub async fn recv(&mut self) -> Option<(HumanPrompt, HumanResponder)> {
        self.rx.recv().await
    }
}

/// Human-in-the-loop handler: pauses execution, presents data to an
/// external operator, waits for their response, then resumes.
///
/// ## Configuration
///
/// - `config.message`: Message to display to the operator (default: "Input required").
///
/// ## Data flow
///
/// - All node inputs are forwarded to the operator as `HumanPrompt.data`.
/// - The operator's response (`Outputs`) becomes the node's outputs.
/// - If the operator drops the responder without responding, the node fails.
///
/// ## Construction
///
/// `HumanInputHandler::new()` returns `(handler, receiver)`. The handler
/// is registered in the handler registry. The receiver is held by the
/// operator (CLI, UI, test harness) to process prompts.
pub struct HumanInputHandler {
    tx: mpsc::Sender<(HumanPrompt, HumanResponder)>,
}

const CHANNEL_CAPACITY: usize = 16;

impl HumanInputHandler {
    /// Create a handler and its corresponding receiver.
    pub fn new() -> (Self, HumanInputReceiver) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        (Self { tx }, HumanInputReceiver { rx })
    }
}

impl NodeHandler for HumanInputHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let tx = self.tx.clone();
        let node_id = node.id.0.clone();
        let label = node.label.clone();
        let message = node
            .config
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Input required")
            .to_string();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before human input".into(),
                });
            }

            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();

            let prompt = HumanPrompt {
                node_id: node_id.clone(),
                label,
                message,
                data: inputs,
            };

            // Send prompt to operator
            tx.send((prompt, HumanResponder { tx: resp_tx }))
                .await
                .map_err(|_| NodeError::Failed {
                    source_message: None,
                    message: format!(
                        "node '{node_id}': human input receiver dropped (no operator listening)"
                    ),
                    recoverable: false,
                })?;

            // Wait for response, respecting cancellation
            tokio::select! {
                result = resp_rx => {
                    result.map_err(|_| NodeError::Failed {
                        source_message: None,
                        message: format!(
                            "node '{node_id}': operator dropped responder without responding"
                        ),
                        recoverable: false,
                    })
                }
                _ = cancel.cancelled() => {
                    Err(NodeError::Cancelled {
                        reason: "cancelled while waiting for human input".into(),
                    })
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn human_input_round_trip() {
        let (handler, mut receiver) = HumanInputHandler::new();

        let mut node = Node::new("REVIEW", "Review Step");
        node.config = serde_json::json!({ "message": "Please approve" });

        let mut inputs = Outputs::new();
        inputs.insert("draft".into(), Value::String("hello world".into()));

        // Spawn operator
        let op = tokio::spawn(async move {
            let (prompt, responder) = receiver.recv().await.unwrap();
            assert_eq!(prompt.node_id, "REVIEW");
            assert_eq!(prompt.message, "Please approve");
            assert_eq!(
                prompt.data.get("draft"),
                Some(&Value::String("hello world".into()))
            );

            let mut response = Outputs::new();
            response.insert("approved".into(), Value::Bool(true));
            responder.respond(response);
        });

        let result = handler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result.get("approved"), Some(&Value::Bool(true)));
        op.await.unwrap();
    }

    #[tokio::test]
    async fn default_message() {
        let (handler, mut receiver) = HumanInputHandler::new();
        let node = Node::new("N", "Node");

        let op = tokio::spawn(async move {
            let (prompt, responder) = receiver.recv().await.unwrap();
            assert_eq!(prompt.message, "Input required");
            responder.respond(Outputs::new());
        });

        handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        op.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_while_waiting() {
        let (handler, _receiver) = HumanInputHandler::new();
        let node = Node::new("N", "Node");

        let token = CancellationToken::new();
        let token2 = token.clone();

        let handle = tokio::spawn(async move {
            handler
                .execute(&node, Outputs::new(), token2)
                .await
        });

        // Cancel after a short delay
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        token.cancel();

        let result = handle.await.unwrap();
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    #[tokio::test]
    async fn cancellation_before_send() {
        let (handler, _receiver) = HumanInputHandler::new();
        let node = Node::new("N", "Node");

        let token = CancellationToken::new();
        token.cancel();

        let result = handler.execute(&node, Outputs::new(), token).await;
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    #[tokio::test]
    async fn receiver_dropped_errors() {
        let (handler, receiver) = HumanInputHandler::new();
        drop(receiver);

        let node = Node::new("N", "Node");
        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no operator listening"));
    }

    #[tokio::test]
    async fn responder_dropped_without_responding() {
        let (handler, mut receiver) = HumanInputHandler::new();
        let node = Node::new("N", "Node");

        // Operator receives but drops responder without calling respond()
        let op = tokio::spawn(async move {
            let (_prompt, _responder) = receiver.recv().await.unwrap();
            // drop responder silently
        });

        let result = handler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("dropped responder"));
        op.await.unwrap();
    }

    #[tokio::test]
    async fn multiple_sequential_prompts() {
        let (handler, mut receiver) = HumanInputHandler::new();

        let op = tokio::spawn(async move {
            for i in 0..3 {
                let (prompt, responder) = receiver.recv().await.unwrap();
                let mut resp = Outputs::new();
                resp.insert("seq".into(), Value::I64(i));
                responder.respond(resp);
            }
        });

        for i in 0..3 {
            let node = Node::new(format!("N{i}"), format!("Step {i}"));
            let result = handler
                .execute(&node, Outputs::new(), CancellationToken::new())
                .await
                .unwrap();
            assert_eq!(result.get("seq"), Some(&Value::I64(i)));
        }

        op.await.unwrap();
    }
}
