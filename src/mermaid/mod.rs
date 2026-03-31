pub mod annotation;
pub mod export;
pub mod loader;
pub mod parse;

pub use export::export_mermaid;
pub use loader::load_mermaid;
pub use parse::{
    ParsedAnnotation, ParsedMermaid, ParsedNode, ParsedSubgraph, Span,
};

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
