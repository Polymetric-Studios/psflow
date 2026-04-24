pub mod annotation;
pub mod export;
pub mod loader;
pub mod parse;

pub use export::export_mermaid;
pub use loader::load_mermaid;
pub use parse::{ParsedAnnotation, ParsedMermaid, ParsedNode, ParsedSubgraph, Span};

use std::fmt;
use std::ops::Deref;
use thiserror::Error;

/// Errors from Mermaid parsing, annotation processing, or graph construction.
#[derive(Debug, Clone, Error)]
pub enum MermaidError {
    #[error("line {line}: {message}")]
    Parse { line: usize, message: String },

    #[error("annotation for '{node_id}': {message}")]
    Annotation { node_id: String, message: String },

    #[error("{0}")]
    Graph(#[from] crate::error::GraphError),
}

/// A collection of [`MermaidError`]s returned from [`load_mermaid`].
///
/// Implements [`std::error::Error`] so callers can use `?` directly.
/// Dereferences to `[MermaidError]` for iteration without an extra accessor.
#[derive(Debug, Clone)]
pub struct MermaidErrors(Vec<MermaidError>);

impl fmt::Display for MermaidErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "{e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for MermaidErrors {}

impl Deref for MermaidErrors {
    type Target = [MermaidError];
    fn deref(&self) -> &[MermaidError] {
        &self.0
    }
}

impl From<Vec<MermaidError>> for MermaidErrors {
    fn from(v: Vec<MermaidError>) -> Self {
        Self(v)
    }
}

impl From<MermaidError> for MermaidErrors {
    fn from(e: MermaidError) -> Self {
        Self(vec![e])
    }
}

impl<'a> IntoIterator for &'a MermaidErrors {
    type Item = &'a MermaidError;
    type IntoIter = std::slice::Iter<'a, MermaidError>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}
