use thiserror::Error;

/// Errors raised by a [`super::SecretResolver`].
#[derive(Debug, Clone, Error, PartialEq)]
pub enum SecretError {
    #[error("secret not found: strategy={strategy}, logical_name={logical_name}")]
    NotFound {
        strategy: String,
        logical_name: String,
    },

    #[error("secret backend failure: {message}")]
    Backend {
        message: String,
        /// True if retrying the same request could plausibly succeed
        /// (e.g. transient network error talking to a vault).
        recoverable: bool,
    },
}

/// Errors raised by auth strategy construction or application.
#[derive(Debug, Clone, Error, PartialEq)]
pub enum AuthError {
    #[error("auth strategy '{name}' not declared in graph metadata")]
    UndeclaredStrategy { name: String },

    #[error("auth strategy type '{type_}' not registered")]
    UnknownStrategyType { type_: String },

    #[error("auth strategy '{name}': {message}")]
    Config { name: String, message: String },

    #[error("auth strategy '{name}': missing required role '{role}' in secrets map")]
    MissingRole { name: String, role: String },

    #[error(transparent)]
    Secret(#[from] SecretError),

    #[error("auth strategy '{name}': template interpolation failed: {message}")]
    Template { name: String, message: String },

    #[error("auth strategy '{name}': apply failed: {message}")]
    Apply { name: String, message: String },
}

impl AuthError {
    pub fn is_recoverable(&self) -> bool {
        match self {
            AuthError::Secret(SecretError::Backend { recoverable, .. }) => *recoverable,
            _ => false,
        }
    }
}
