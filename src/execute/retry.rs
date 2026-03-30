use crate::error::NodeError;
use crate::execute::context::ExecutionContext;
use crate::execute::event::ExecutionEvent;
use crate::execute::{CancellationToken, NodeHandler, Outputs};
use crate::graph::node::Node;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

/// Backoff strategy for retries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BackoffStrategy {
    /// Fixed delay between attempts.
    Fixed { delay_ms: u64 },
    /// Exponential backoff: `initial_delay_ms * multiplier^(attempt-1)`, capped at `max_delay_ms`.
    Exponential {
        initial_delay_ms: u64,
        multiplier: f64,
        max_delay_ms: u64,
    },
}

/// Retry configuration parsed from `exec.retry.*` annotation keys.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub backoff: BackoffStrategy,
    /// Per-attempt timeout (separate from the node-level `exec.timeout_ms`).
    pub attempt_timeout_ms: Option<u64>,
    /// Whether to retry on `NodeError::Timeout`.
    pub retry_on_timeout: bool,
}

impl RetryConfig {
    /// Parse retry config from a node's `exec` JSON object.
    ///
    /// Returns `None` if no `retry.max_attempts` key exists or if max_attempts <= 1.
    pub fn from_exec(exec: &serde_json::Value) -> Option<Self> {
        let retry = exec.get("retry")?;

        let max_attempts = retry
            .get("max_attempts")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .min(100) as u32; // clamped to prevent absurd retry counts

        if max_attempts <= 1 {
            return None;
        }

        let backoff_type = retry
            .get("backoff")
            .and_then(|v| v.as_str())
            .unwrap_or("fixed");

        let delay_ms = retry
            .get("delay_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(100);

        let backoff = match backoff_type {
            "exponential" => {
                let multiplier = retry
                    .get("multiplier")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(2.0);
                let max_delay_ms = retry
                    .get("max_delay_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(60_000);
                BackoffStrategy::Exponential {
                    initial_delay_ms: delay_ms,
                    multiplier,
                    max_delay_ms,
                }
            }
            _ => BackoffStrategy::Fixed { delay_ms },
        };

        let attempt_timeout_ms = retry
            .get("attempt_timeout_ms")
            .and_then(|v| v.as_u64());

        let retry_on_timeout = retry
            .get("retry_on_timeout")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        Some(Self {
            max_attempts,
            backoff,
            attempt_timeout_ms,
            retry_on_timeout,
        })
    }

    /// Compute the delay before the given attempt (0-indexed, so attempt 0 = first retry wait).
    pub fn delay_for_attempt(&self, attempt: u32) -> Duration {
        let ms = match &self.backoff {
            BackoffStrategy::Fixed { delay_ms } => *delay_ms,
            BackoffStrategy::Exponential {
                initial_delay_ms,
                multiplier,
                max_delay_ms,
            } => {
                // Clamp exponent to avoid f64 overflow for large attempt counts
                let clamped = attempt.min(63);
                let delay = (*initial_delay_ms as f64) * multiplier.powi(clamped as i32);
                let delay_ms = if delay.is_finite() { delay as u64 } else { *max_delay_ms };
                delay_ms.min(*max_delay_ms)
            }
        };
        Duration::from_millis(ms)
    }
}

/// Execute a handler with retry logic.
///
/// On each attempt:
/// 1. Check cancellation
/// 2. Call `handler.execute()` with optional per-attempt timeout
/// 3. On success → return outputs
/// 4. On retryable error → emit `NodeRetrying` event, wait backoff delay (cancellable)
/// 5. On non-retryable error or last attempt → return error
pub async fn execute_with_retry(
    handler: &Arc<dyn NodeHandler>,
    node: &Node,
    inputs: Outputs,
    cancel: CancellationToken,
    config: &RetryConfig,
) -> Result<Outputs, NodeError> {
    execute_with_retry_ctx(handler, node, inputs, cancel, config, None).await
}

/// Execute with retry, optionally emitting `NodeRetrying` events to the execution context.
pub async fn execute_with_retry_ctx(
    handler: &Arc<dyn NodeHandler>,
    node: &Node,
    inputs: Outputs,
    cancel: CancellationToken,
    config: &RetryConfig,
    ctx: Option<&ExecutionContext>,
) -> Result<Outputs, NodeError> {
    let mut last_error = None;

    for attempt in 0..config.max_attempts {
        if cancel.is_cancelled() {
            return Err(NodeError::Cancelled {
                reason: "cancelled before retry attempt".into(),
            });
        }

        // Execute with optional per-attempt timeout
        let result = if let Some(timeout_ms) = config.attempt_timeout_ms {
            let timeout = Duration::from_millis(timeout_ms);
            match tokio::time::timeout(
                timeout,
                handler.execute(node, inputs.clone(), cancel.clone()),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => Err(NodeError::Timeout {
                    elapsed_ms: timeout_ms,
                    limit_ms: timeout_ms,
                }),
            }
        } else {
            handler.execute(node, inputs.clone(), cancel.clone()).await
        };

        match result {
            Ok(outputs) => return Ok(outputs),
            Err(ref e) if attempt + 1 < config.max_attempts && is_retryable(e, config) => {
                let delay = config.delay_for_attempt(attempt);
                let error = result.unwrap_err();

                warn!(
                    node = %node.id.0,
                    attempt = attempt + 1,
                    max_attempts = config.max_attempts,
                    delay_ms = delay.as_millis() as u64,
                    error = %error,
                    "retrying node"
                );

                // Emit retry event
                if let Some(ctx) = ctx {
                    ctx.emit(ExecutionEvent::NodeRetrying {
                        node_id: node.id.0.clone(),
                        attempt: attempt + 1,
                        max_attempts: config.max_attempts,
                        error: error.clone(),
                        next_delay_ms: delay.as_millis() as u64,
                    });
                }

                last_error = Some(error);

                // Wait with cancellation support
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancel.cancelled() => {
                        return Err(NodeError::Cancelled {
                            reason: "cancelled during retry backoff".into(),
                        });
                    }
                }
            }
            Err(e) => {
                return Err(e);
            }
        }
    }

    Err(last_error.unwrap_or(NodeError::Failed {
        source_message: None,
        message: "retry exhausted with no error".into(),
        recoverable: false,
    }))
}

/// Whether a `NodeError` should trigger a retry.
fn is_retryable(error: &NodeError, config: &RetryConfig) -> bool {
    match error {
        NodeError::Failed {
            recoverable: true, ..
        } => true,
        NodeError::Timeout { .. } => config.retry_on_timeout,
        NodeError::AdapterError { .. } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute::sync_handler;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn parse_fixed_retry_config() {
        let exec = serde_json::json!({
            "retry": {
                "max_attempts": 3,
                "delay_ms": 500
            }
        });
        let config = RetryConfig::from_exec(&exec).unwrap();
        assert_eq!(config.max_attempts, 3);
        assert_eq!(config.backoff, BackoffStrategy::Fixed { delay_ms: 500 });
        assert!(config.retry_on_timeout);
    }

    #[test]
    fn parse_exponential_retry_config() {
        let exec = serde_json::json!({
            "retry": {
                "max_attempts": 5,
                "backoff": "exponential",
                "delay_ms": 100,
                "multiplier": 2.0,
                "max_delay_ms": 5000,
                "attempt_timeout_ms": 3000,
                "retry_on_timeout": false
            }
        });
        let config = RetryConfig::from_exec(&exec).unwrap();
        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.attempt_timeout_ms, Some(3000));
        assert!(!config.retry_on_timeout);
        assert!(matches!(
            config.backoff,
            BackoffStrategy::Exponential {
                initial_delay_ms: 100,
                multiplier,
                max_delay_ms: 5000
            } if (multiplier - 2.0).abs() < f64::EPSILON
        ));
    }

    #[test]
    fn parse_returns_none_for_no_retry() {
        assert!(RetryConfig::from_exec(&serde_json::json!({})).is_none());
        assert!(RetryConfig::from_exec(&serde_json::json!({"retry": {"max_attempts": 1}})).is_none());
    }

    #[test]
    fn delay_fixed() {
        let config = RetryConfig {
            max_attempts: 3,
            backoff: BackoffStrategy::Fixed { delay_ms: 200 },
            attempt_timeout_ms: None,
            retry_on_timeout: true,
        };
        assert_eq!(config.delay_for_attempt(0), Duration::from_millis(200));
        assert_eq!(config.delay_for_attempt(1), Duration::from_millis(200));
        assert_eq!(config.delay_for_attempt(5), Duration::from_millis(200));
    }

    #[test]
    fn delay_exponential() {
        let config = RetryConfig {
            max_attempts: 5,
            backoff: BackoffStrategy::Exponential {
                initial_delay_ms: 100,
                multiplier: 2.0,
                max_delay_ms: 1000,
            },
            attempt_timeout_ms: None,
            retry_on_timeout: true,
        };
        assert_eq!(config.delay_for_attempt(0), Duration::from_millis(100)); // 100 * 2^0
        assert_eq!(config.delay_for_attempt(1), Duration::from_millis(200)); // 100 * 2^1
        assert_eq!(config.delay_for_attempt(2), Duration::from_millis(400)); // 100 * 2^2
        assert_eq!(config.delay_for_attempt(3), Duration::from_millis(800)); // 100 * 2^3
        assert_eq!(config.delay_for_attempt(4), Duration::from_millis(1000)); // capped
    }

    #[test]
    fn is_retryable_classifications() {
        let config = RetryConfig {
            max_attempts: 3,
            backoff: BackoffStrategy::Fixed { delay_ms: 0 },
            attempt_timeout_ms: None,
            retry_on_timeout: true,
        };

        assert!(is_retryable(
            &NodeError::Failed {
                source_message: None,
                message: "x".into(),
                recoverable: true,
            },
            &config
        ));
        assert!(!is_retryable(
            &NodeError::Failed {
                source_message: None,
                message: "x".into(),
                recoverable: false,
            },
            &config
        ));
        assert!(is_retryable(
            &NodeError::Timeout {
                elapsed_ms: 100,
                limit_ms: 50,
            },
            &config
        ));
        assert!(is_retryable(
            &NodeError::AdapterError {
                adapter: "x".into(),
                message: "x".into(),
            },
            &config
        ));
        assert!(!is_retryable(
            &NodeError::Cancelled {
                reason: "x".into(),
            },
            &config
        ));
    }

    #[test]
    fn is_retryable_timeout_disabled() {
        let config = RetryConfig {
            max_attempts: 3,
            backoff: BackoffStrategy::Fixed { delay_ms: 0 },
            attempt_timeout_ms: None,
            retry_on_timeout: false,
        };
        assert!(!is_retryable(
            &NodeError::Timeout {
                elapsed_ms: 100,
                limit_ms: 50,
            },
            &config
        ));
    }

    #[tokio::test]
    async fn retry_succeeds_on_second_attempt() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let handler: Arc<dyn NodeHandler> = sync_handler(move |_, _| {
            let n = counter_clone.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Err(NodeError::Failed {
                    source_message: None,
                    message: "transient".into(),
                    recoverable: true,
                })
            } else {
                let mut out = Outputs::new();
                out.insert("ok".into(), crate::graph::types::Value::Bool(true));
                Ok(out)
            }
        });

        let config = RetryConfig {
            max_attempts: 3,
            backoff: BackoffStrategy::Fixed { delay_ms: 1 },
            attempt_timeout_ms: None,
            retry_on_timeout: true,
        };

        let result = execute_with_retry(
            &handler,
            &Node::new("N", "N"),
            Outputs::new(),
            CancellationToken::new(),
            &config,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retry_non_recoverable_fails_immediately() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let handler: Arc<dyn NodeHandler> = sync_handler(move |_, _| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            Err(NodeError::Failed {
                source_message: None,
                message: "fatal".into(),
                recoverable: false,
            })
        });

        let config = RetryConfig {
            max_attempts: 5,
            backoff: BackoffStrategy::Fixed { delay_ms: 1 },
            attempt_timeout_ms: None,
            retry_on_timeout: true,
        };

        let result = execute_with_retry(
            &handler,
            &Node::new("N", "N"),
            Outputs::new(),
            CancellationToken::new(),
            &config,
        )
        .await;

        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 1); // no retry
    }

    #[tokio::test]
    async fn retry_exhausted_returns_last_error() {
        let handler: Arc<dyn NodeHandler> = sync_handler(|_, _| {
            Err(NodeError::Failed {
                source_message: None,
                message: "always fails".into(),
                recoverable: true,
            })
        });

        let config = RetryConfig {
            max_attempts: 3,
            backoff: BackoffStrategy::Fixed { delay_ms: 1 },
            attempt_timeout_ms: None,
            retry_on_timeout: true,
        };

        let result = execute_with_retry(
            &handler,
            &Node::new("N", "N"),
            Outputs::new(),
            CancellationToken::new(),
            &config,
        )
        .await;

        assert!(matches!(result, Err(NodeError::Failed { .. })));
    }

    #[tokio::test]
    async fn retry_cancelled_during_backoff() {
        let handler: Arc<dyn NodeHandler> = sync_handler(|_, _| {
            Err(NodeError::Failed {
                source_message: None,
                message: "fail".into(),
                recoverable: true,
            })
        });

        let config = RetryConfig {
            max_attempts: 10,
            backoff: BackoffStrategy::Fixed { delay_ms: 5000 },
            attempt_timeout_ms: None,
            retry_on_timeout: true,
        };

        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();

        let handle = tokio::spawn(async move {
            execute_with_retry(
                &handler,
                &Node::new("N", "N"),
                Outputs::new(),
                cancel2,
                &config,
            )
            .await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();

        let result = handle.await.unwrap();
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    #[tokio::test]
    async fn no_retry_on_success() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();
        let handler: Arc<dyn NodeHandler> = sync_handler(move |_, inputs| {
            counter_clone.fetch_add(1, Ordering::SeqCst);
            Ok(inputs)
        });

        let config = RetryConfig {
            max_attempts: 5,
            backoff: BackoffStrategy::Fixed { delay_ms: 1 },
            attempt_timeout_ms: None,
            retry_on_timeout: true,
        };

        let result = execute_with_retry(
            &handler,
            &Node::new("N", "N"),
            Outputs::new(),
            CancellationToken::new(),
            &config,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 1); // single attempt
    }
}
