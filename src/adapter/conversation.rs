//! Conversation history for LLM context accumulation.
//!
//! Provides a message list that stateless adapters can use to maintain
//! conversational context across multiple LLM calls in a graph execution.

use serde::{Deserialize, Serialize};

/// Role of a message in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

impl std::fmt::Display for MessageRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageRole::User => write!(f, "user"),
            MessageRole::Assistant => write!(f, "assistant"),
            MessageRole::System => write!(f, "system"),
        }
    }
}

/// A single message in a conversation history.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationMessage {
    /// The message role.
    pub role: MessageRole,
    /// The message content.
    pub content: String,
    /// The node that produced this message (for traceability).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

impl ConversationMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            node_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            node_id: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            node_id: None,
        }
    }

    pub fn with_node(mut self, node_id: impl Into<String>) -> Self {
        self.node_id = Some(node_id.into());
        self
    }

    /// Rough token estimate: ~4 chars per token (conservative).
    pub fn estimated_tokens(&self) -> usize {
        self.content.len() / 4 + 1
    }
}

/// Configuration for conversation history assembly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConversationConfig {
    /// Maximum total tokens for the history. Older messages are dropped first.
    /// Default: no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
    /// Maximum number of ancestor LLM exchanges to include.
    /// Default: no limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<usize>,
}

/// An ordered list of conversation messages, typically accumulated across
/// multiple LLM nodes in a graph execution.
///
/// Stored on the blackboard and passed to stateless adapters as prefix
/// context for each LLM call.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ConversationHistory {
    pub messages: Vec<ConversationMessage>,
}

impl ConversationHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a message to the history.
    pub fn push(&mut self, message: ConversationMessage) {
        self.messages.push(message);
    }

    /// Append a user/assistant pair from an LLM call.
    pub fn push_exchange(
        &mut self,
        node_id: &str,
        prompt: impl Into<String>,
        response: impl Into<String>,
    ) {
        self.push(ConversationMessage::user(prompt).with_node(node_id));
        self.push(ConversationMessage::assistant(response).with_node(node_id));
    }

    /// Total estimated tokens across all messages.
    pub fn estimated_tokens(&self) -> usize {
        self.messages.iter().map(|m| m.estimated_tokens()).sum()
    }

    /// Number of messages.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Truncate to fit within a token budget by dropping oldest messages first.
    ///
    /// Always keeps at least the last message pair (most recent context).
    pub fn truncate_to_budget(&mut self, max_tokens: usize) {
        while self.estimated_tokens() > max_tokens && self.messages.len() > 2 {
            self.messages.remove(0);
        }
    }

    /// Limit to the most recent N message pairs (2*N messages).
    pub fn truncate_to_depth(&mut self, max_depth: usize) {
        let max_messages = max_depth * 2;
        if self.messages.len() > max_messages {
            let start = self.messages.len() - max_messages;
            self.messages = self.messages[start..].to_vec();
        }
    }

    /// Apply a conversation config's limits.
    pub fn apply_limits(&mut self, config: &ConversationConfig) {
        if let Some(depth) = config.max_depth {
            self.truncate_to_depth(depth);
        }
        if let Some(tokens) = config.max_tokens {
            self.truncate_to_budget(tokens);
        }
    }

    /// Convert to the blackboard Value representation for storage.
    pub fn to_value(&self) -> crate::graph::types::Value {
        let json = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        crate::graph::types::Value::from(json)
    }

    /// Restore from a blackboard Value.
    pub fn from_value(value: &crate::graph::types::Value) -> Option<Self> {
        let json = serde_json::Value::from(value);
        serde_json::from_value(json).ok()
    }
}

/// The blackboard key used for conversation history.
pub const CONVERSATION_HISTORY_KEY: &str = "__conversation_history";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_retrieve_messages() {
        let mut history = ConversationHistory::new();
        history.push(ConversationMessage::user("Hello"));
        history.push(ConversationMessage::assistant("Hi there"));

        assert_eq!(history.len(), 2);
        assert_eq!(history.messages[0].role, MessageRole::User);
        assert_eq!(history.messages[1].role, MessageRole::Assistant);
    }

    #[test]
    fn push_exchange() {
        let mut history = ConversationHistory::new();
        history.push_exchange("LLM1", "What is 2+2?", "4");

        assert_eq!(history.len(), 2);
        assert_eq!(history.messages[0].node_id, Some("LLM1".into()));
        assert_eq!(history.messages[0].content, "What is 2+2?");
        assert_eq!(history.messages[1].content, "4");
    }

    #[test]
    fn estimated_tokens() {
        let mut history = ConversationHistory::new();
        // 20 chars ≈ 6 tokens (20/4 + 1)
        history.push(ConversationMessage::user("12345678901234567890"));
        assert!(history.estimated_tokens() > 0);
    }

    #[test]
    fn truncate_to_budget() {
        let mut history = ConversationHistory::new();
        for i in 0..10 {
            history.push_exchange(
                &format!("N{i}"),
                "x".repeat(100), // ~26 tokens each
                "y".repeat(100),
            );
        }

        let before = history.len();
        history.truncate_to_budget(100); // Very tight budget
        assert!(history.len() < before);
        assert!(history.len() >= 2); // Always keeps last pair
    }

    #[test]
    fn truncate_to_depth() {
        let mut history = ConversationHistory::new();
        for i in 0..10 {
            history.push_exchange(&format!("N{i}"), "prompt", "response");
        }

        history.truncate_to_depth(3);
        assert_eq!(history.len(), 6); // 3 pairs × 2 messages

        // Should keep the most recent 3 pairs
        assert_eq!(history.messages[0].node_id, Some("N7".into()));
        assert_eq!(history.messages[4].node_id, Some("N9".into()));
    }

    #[test]
    fn apply_limits() {
        let mut history = ConversationHistory::new();
        for i in 0..10 {
            history.push_exchange(&format!("N{i}"), "prompt", "response");
        }

        let config = ConversationConfig {
            max_depth: Some(2),
            ..Default::default()
        };
        history.apply_limits(&config);
        assert_eq!(history.len(), 4); // 2 pairs
    }

    #[test]
    fn serde_round_trip() {
        let mut history = ConversationHistory::new();
        history.push(ConversationMessage::system("You are helpful"));
        history.push_exchange("A", "Hello", "Hi");

        let json = serde_json::to_string(&history).unwrap();
        let parsed: ConversationHistory = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, history);
    }

    #[test]
    fn value_round_trip() {
        let mut history = ConversationHistory::new();
        history.push_exchange("A", "prompt", "response");

        let value = history.to_value();
        let restored = ConversationHistory::from_value(&value).unwrap();
        assert_eq!(restored, history);
    }

    #[test]
    fn empty_history() {
        let history = ConversationHistory::new();
        assert!(history.is_empty());
        assert_eq!(history.len(), 0);
        assert_eq!(history.estimated_tokens(), 0);
    }

    #[test]
    fn default_config() {
        let config = ConversationConfig::default();
        assert!(config.max_tokens.is_none());
        assert!(config.max_depth.is_none());
    }
}
