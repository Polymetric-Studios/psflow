use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Concurrency limits for graph execution.
///
/// Controls the maximum number of nodes executing concurrently.
/// When a limit is set, tasks acquire a semaphore permit before
/// executing and release it on completion. Without a limit,
/// all tasks spawn immediately (current default behavior).
#[derive(Debug, Clone)]
pub struct ConcurrencyLimits {
    global: Option<Arc<Semaphore>>,
}

impl ConcurrencyLimits {
    /// No concurrency limits (unlimited parallelism).
    pub fn new() -> Self {
        Self { global: None }
    }

    /// Limit total concurrent node executions across the entire graph.
    pub fn with_max_parallelism(max: usize) -> Self {
        Self {
            global: Some(Arc::new(Semaphore::new(max))),
        }
    }

    /// Acquire a global permit. Returns `None` if no limit is set.
    /// Blocks until a permit is available.
    pub async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        if let Some(ref sem) = self.global {
            Some(
                sem.clone()
                    .acquire_owned()
                    .await
                    .expect("semaphore closed unexpectedly"),
            )
        } else {
            None
        }
    }

    /// Whether a global limit is configured.
    pub fn is_limited(&self) -> bool {
        self.global.is_some()
    }
}

impl Default for ConcurrencyLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a semaphore for per-subgraph concurrency limiting.
pub fn subgraph_semaphore(max_concurrent: usize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(max_concurrent))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_by_default() {
        let limits = ConcurrencyLimits::new();
        assert!(!limits.is_limited());
    }

    #[test]
    fn with_max_parallelism_is_limited() {
        let limits = ConcurrencyLimits::with_max_parallelism(5);
        assert!(limits.is_limited());
    }

    #[tokio::test]
    async fn acquire_returns_none_when_unlimited() {
        let limits = ConcurrencyLimits::new();
        assert!(limits.acquire().await.is_none());
    }

    #[tokio::test]
    async fn acquire_returns_permit_when_limited() {
        let limits = ConcurrencyLimits::with_max_parallelism(2);
        let p1 = limits.acquire().await;
        assert!(p1.is_some());
        let p2 = limits.acquire().await;
        assert!(p2.is_some());
        // Both permits held — a third would block, but we won't test blocking here
    }

    #[tokio::test]
    async fn permits_released_on_drop() {
        let limits = ConcurrencyLimits::with_max_parallelism(1);
        {
            let _p = limits.acquire().await;
            // permit held
        }
        // permit dropped — should be available again
        let p = limits.acquire().await;
        assert!(p.is_some());
    }

    #[test]
    fn subgraph_semaphore_creates_with_capacity() {
        let sem = subgraph_semaphore(3);
        // Can acquire 3 permits without blocking
        let _p1 = sem.try_acquire().unwrap();
        let _p2 = sem.try_acquire().unwrap();
        let _p3 = sem.try_acquire().unwrap();
        assert!(sem.try_acquire().is_err()); // 4th blocked
    }
}
