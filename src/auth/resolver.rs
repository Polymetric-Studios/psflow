use super::error::SecretError;
use super::secret::SecretValue;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

/// Structured lookup key passed to [`SecretResolver::resolve`].
///
/// `strategy_name` is the graph-local strategy name (useful for per-strategy
/// vault paths or audit tags); `logical_name` is the host-side identifier
/// the strategy config's `secrets` map maps its role name to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRequest {
    pub strategy_name: String,
    pub logical_name: String,
}

impl SecretRequest {
    pub fn new(strategy_name: impl Into<String>, logical_name: impl Into<String>) -> Self {
        Self {
            strategy_name: strategy_name.into(),
            logical_name: logical_name.into(),
        }
    }
}

/// Host-provided secret lookup interface.
///
/// The contract: given a [`SecretRequest`], return the current value of the
/// secret. psflow does not memoize — hosts wanting caching wrap their own
/// resolver. psflow does not panic on missing secrets; it surfaces
/// [`SecretError::NotFound`] which strategies translate to
/// [`super::AuthError::Secret`].
#[async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, request: &SecretRequest) -> Result<SecretValue, SecretError>;
}

/// A resolver that always errors. Useful as a default when a graph never
/// exercises auth, and as a guard against accidentally leaving a resolver
/// unset.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSecretResolver;

#[async_trait]
impl SecretResolver for NullSecretResolver {
    async fn resolve(&self, request: &SecretRequest) -> Result<SecretValue, SecretError> {
        Err(SecretError::NotFound {
            strategy: request.strategy_name.clone(),
            logical_name: request.logical_name.clone(),
        })
    }
}

/// An in-memory resolver backed by a `HashMap`. Intended for tests and
/// simple embedders that load secrets from env vars at startup.
pub struct StaticSecretResolver {
    // Interior mutability lets callers `insert` through a shared `Arc`.
    inner: Mutex<HashMap<String, Vec<u8>>>,
}

impl StaticSecretResolver {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Key format: `"{strategy_name}:{logical_name}"`. Host callers can
    /// also use [`Self::insert_flat`] when the strategy name is irrelevant.
    pub fn insert(
        &self,
        strategy_name: &str,
        logical_name: &str,
        value: impl Into<Vec<u8>>,
    ) -> &Self {
        let key = format!("{strategy_name}:{logical_name}");
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, value.into());
        self
    }

    /// Insert a secret under just the logical name — matches the convention
    /// where the host ignores `strategy_name` and uses a flat key space.
    pub fn insert_flat(&self, logical_name: &str, value: impl Into<Vec<u8>>) -> &Self {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(logical_name.to_string(), value.into());
        self
    }
}

impl Default for StaticSecretResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SecretResolver for StaticSecretResolver {
    async fn resolve(&self, request: &SecretRequest) -> Result<SecretValue, SecretError> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let key = format!("{}:{}", request.strategy_name, request.logical_name);
        if let Some(bytes) = guard.get(&key) {
            return Ok(SecretValue::new(bytes.clone()));
        }
        if let Some(bytes) = guard.get(&request.logical_name) {
            return Ok(SecretValue::new(bytes.clone()));
        }
        Err(SecretError::NotFound {
            strategy: request.strategy_name.clone(),
            logical_name: request.logical_name.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn null_resolver_always_errors() {
        let r = NullSecretResolver;
        let err = r.resolve(&SecretRequest::new("s", "l")).await.unwrap_err();
        assert!(matches!(err, SecretError::NotFound { .. }));
    }

    #[tokio::test]
    async fn static_resolver_scoped_lookup() {
        let r = StaticSecretResolver::new();
        r.insert("bearer1", "token", "v1");
        r.insert("bearer2", "token", "v2");

        let a = r
            .resolve(&SecretRequest::new("bearer1", "token"))
            .await
            .unwrap();
        assert_eq!(a.reveal_str(), Some("v1"));

        let b = r
            .resolve(&SecretRequest::new("bearer2", "token"))
            .await
            .unwrap();
        assert_eq!(b.reveal_str(), Some("v2"));
    }

    #[tokio::test]
    async fn static_resolver_flat_fallback() {
        let r = StaticSecretResolver::new();
        r.insert_flat("token", "shared");

        let v = r
            .resolve(&SecretRequest::new("any_strategy", "token"))
            .await
            .unwrap();
        assert_eq!(v.reveal_str(), Some("shared"));
    }

    #[tokio::test]
    async fn static_resolver_missing_errors() {
        let r = StaticSecretResolver::new();
        let err = r
            .resolve(&SecretRequest::new("s", "missing"))
            .await
            .unwrap_err();
        assert!(matches!(err, SecretError::NotFound { .. }));
    }
}
