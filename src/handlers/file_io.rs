use crate::error::NodeError;
use crate::execute::{CancellationToken, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::handlers::common::{interpolate, validate_path_containment};
use std::future::Future;
use std::pin::Pin;

/// Read a file's contents.
///
/// ## Configuration
/// - `config.path` (required): File path template with `{key}` interpolation from inputs.
/// - `config.base_dir`: If set, resolved paths must stay within this directory (path traversal protection).
///
/// ## Outputs
/// - `content`: File contents as String
/// - `path`: The resolved file path
pub struct ReadFileHandler;

impl NodeHandler for ReadFileHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let config = node.config.clone();
        let node_id = node.id.0.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before file read".into(),
                });
            }

            let path_template = config
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': missing config.path"),
                    recoverable: false,
                })?;

            let path = interpolate(path_template, &inputs);

            // Path containment check
            if let Some(base_dir) = config.get("base_dir").and_then(|v| v.as_str()) {
                let safe_path = validate_path_containment(&path, base_dir).map_err(|e| {
                    NodeError::Failed {
                        source_message: None,
                        message: format!("node '{node_id}': {e}"),
                        recoverable: false,
                    }
                })?;
                // Use the normalized path
                let path = safe_path.to_string_lossy().to_string();
                let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                    NodeError::Failed {
                        source_message: Some(e.to_string()),
                        message: format!("node '{node_id}': failed to read '{path}': {e}"),
                        recoverable: false,
                    }
                })?;
                let mut outputs = Outputs::new();
                outputs.insert("content".into(), Value::String(content));
                outputs.insert("path".into(), Value::String(path));
                return Ok(outputs);
            }

            let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                NodeError::Failed {
                    source_message: Some(e.to_string()),
                    message: format!("node '{node_id}': failed to read '{path}': {e}"),
                    recoverable: false,
                }
            })?;

            let mut outputs = Outputs::new();
            outputs.insert("content".into(), Value::String(content));
            outputs.insert("path".into(), Value::String(path));
            Ok(outputs)
        })
    }
}

/// Write content to a file.
///
/// ## Configuration
/// - `config.path` (required): File path template with `{key}` interpolation.
/// - `config.create_dirs`: If true (default), create parent directories as needed.
/// - `config.input_key`: Key to read content from inputs (default: `"content"`).
/// - `config.base_dir`: If set, resolved paths must stay within this directory (path traversal protection).
///
/// ## Outputs
/// - `path`: The resolved file path
/// - `bytes_written`: Number of bytes written (i64)
pub struct WriteFileHandler;

impl NodeHandler for WriteFileHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let config = node.config.clone();
        let node_id = node.id.0.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before file write".into(),
                });
            }

            let path_template = config
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': missing config.path"),
                    recoverable: false,
                })?;

            let input_key = config
                .get("input_key")
                .and_then(|v| v.as_str())
                .unwrap_or("content");

            let create_dirs = config
                .get("create_dirs")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            let path = interpolate(path_template, &inputs);

            // Path containment check
            let path = if let Some(base_dir) = config.get("base_dir").and_then(|v| v.as_str()) {
                validate_path_containment(&path, base_dir)
                    .map_err(|e| NodeError::Failed {
                        source_message: None,
                        message: format!("node '{node_id}': {e}"),
                        recoverable: false,
                    })?
                    .to_string_lossy()
                    .to_string()
            } else {
                path
            };

            let content = inputs
                .get(input_key)
                .map(|v| match v {
                    Value::String(s) => s.clone(),
                    other => format!("{other:?}"),
                })
                .unwrap_or_default();

            // Create parent directories if needed
            if create_dirs {
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    if !parent.as_os_str().is_empty() {
                        tokio::fs::create_dir_all(parent).await.map_err(|e| {
                            NodeError::Failed {
                                source_message: Some(e.to_string()),
                                message: format!(
                                    "node '{node_id}': failed to create dirs for '{path}': {e}"
                                ),
                                recoverable: false,
                            }
                        })?;
                    }
                }
            }

            let bytes = content.as_bytes().len() as i64;
            tokio::fs::write(&path, &content).await.map_err(|e| {
                NodeError::Failed {
                    source_message: Some(e.to_string()),
                    message: format!("node '{node_id}': failed to write '{path}': {e}"),
                    recoverable: false,
                }
            })?;

            let mut outputs = Outputs::new();
            outputs.insert("path".into(), Value::String(path));
            outputs.insert("bytes_written".into(), Value::I64(bytes));
            Ok(outputs)
        })
    }
}

/// List files matching a glob pattern.
///
/// ## Configuration
/// - `config.pattern` (required): Glob pattern, e.g. `"./data/*.json"`.
///   Supports `{key}` interpolation from inputs.
///
/// ## Outputs
/// - `files`: Vec of matching file paths (as strings)
/// - `count`: Number of matches (i64)
pub struct GlobHandler;

impl NodeHandler for GlobHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let config = node.config.clone();
        let node_id = node.id.0.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before glob".into(),
                });
            }

            let pattern_template = config
                .get("pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': missing config.pattern"),
                    recoverable: false,
                })?;

            let pattern = interpolate(pattern_template, &inputs);

            let paths: Vec<Value> = glob::glob(&pattern)
                .map_err(|e| NodeError::Failed {
                    source_message: Some(e.to_string()),
                    message: format!("node '{node_id}': invalid glob pattern '{pattern}': {e}"),
                    recoverable: false,
                })?
                .filter_map(|entry| entry.ok())
                .map(|p| Value::String(p.to_string_lossy().to_string()))
                .collect();

            let count = paths.len() as i64;

            let mut outputs = Outputs::new();
            outputs.insert("files".into(), Value::Vec(paths));
            outputs.insert("count".into(), Value::I64(count));
            Ok(outputs)
        })
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_file_success() {
        let dir = std::env::temp_dir().join("psflow_test_read");
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file_path = dir.join("test.txt");
        tokio::fs::write(&file_path, "hello world").await.unwrap();

        let mut node = Node::new("R", "Read");
        node.config = serde_json::json!({ "path": file_path.to_str().unwrap() });

        let result = ReadFileHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result["content"], Value::String("hello world".into()));
        assert_eq!(
            result["path"],
            Value::String(file_path.to_str().unwrap().to_string())
        );

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn read_file_missing_errors() {
        let mut node = Node::new("R", "Read");
        node.config = serde_json::json!({ "path": "/nonexistent/file.txt" });

        let result = ReadFileHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn read_file_missing_config_errors() {
        let node = Node::new("R", "Read");
        let result = ReadFileHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing config.path"));
    }

    #[tokio::test]
    async fn read_file_path_interpolation() {
        let dir = std::env::temp_dir().join("psflow_test_read_interp");
        let _ = tokio::fs::create_dir_all(&dir).await;
        let file_path = dir.join("data.txt");
        tokio::fs::write(&file_path, "interpolated").await.unwrap();

        let mut node = Node::new("R", "Read");
        let template = format!("{}/{{filename}}", dir.to_str().unwrap());
        node.config = serde_json::json!({ "path": template });

        let mut inputs = Outputs::new();
        inputs.insert("filename".into(), Value::String("data.txt".into()));

        let result = ReadFileHandler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result["content"], Value::String("interpolated".into()));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn write_file_success() {
        let dir = std::env::temp_dir().join("psflow_test_write");
        let file_path = dir.join("output.txt");

        let mut node = Node::new("W", "Write");
        node.config = serde_json::json!({ "path": file_path.to_str().unwrap() });

        let mut inputs = Outputs::new();
        inputs.insert("content".into(), Value::String("written data".into()));

        let result = WriteFileHandler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result["bytes_written"], Value::I64(12));

        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "written data");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn write_file_creates_dirs() {
        let dir = std::env::temp_dir().join("psflow_test_write_dirs/nested/deep");
        let file_path = dir.join("file.txt");

        let mut node = Node::new("W", "Write");
        node.config = serde_json::json!({ "path": file_path.to_str().unwrap() });

        let mut inputs = Outputs::new();
        inputs.insert("content".into(), Value::String("deep".into()));

        let result = WriteFileHandler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result["bytes_written"], Value::I64(4));
        assert!(file_path.exists());

        let _ = tokio::fs::remove_dir_all(
            std::env::temp_dir().join("psflow_test_write_dirs"),
        )
        .await;
    }

    #[tokio::test]
    async fn write_file_custom_input_key() {
        let dir = std::env::temp_dir().join("psflow_test_write_key");
        let file_path = dir.join("custom.txt");

        let mut node = Node::new("W", "Write");
        node.config = serde_json::json!({
            "path": file_path.to_str().unwrap(),
            "input_key": "data"
        });

        let mut inputs = Outputs::new();
        inputs.insert("data".into(), Value::String("custom key".into()));

        let result = WriteFileHandler
            .execute(&node, inputs, CancellationToken::new())
            .await
            .unwrap();

        let content = tokio::fs::read_to_string(&file_path).await.unwrap();
        assert_eq!(content, "custom key");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn glob_finds_files() {
        let dir = std::env::temp_dir().join("psflow_test_glob");
        let _ = tokio::fs::create_dir_all(&dir).await;
        tokio::fs::write(dir.join("a.txt"), "").await.unwrap();
        tokio::fs::write(dir.join("b.txt"), "").await.unwrap();
        tokio::fs::write(dir.join("c.json"), "").await.unwrap();

        let mut node = Node::new("G", "Glob");
        let pattern = format!("{}/*.txt", dir.to_str().unwrap());
        node.config = serde_json::json!({ "pattern": pattern });

        let result = GlobHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result["count"], Value::I64(2));
        match &result["files"] {
            Value::Vec(files) => assert_eq!(files.len(), 2),
            other => panic!("expected Vec, got {other:?}"),
        }

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn glob_no_matches() {
        let mut node = Node::new("G", "Glob");
        node.config = serde_json::json!({ "pattern": "/nonexistent_dir_xyz/*.nothing" });

        let result = GlobHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(result["count"], Value::I64(0));
    }

    #[tokio::test]
    async fn glob_missing_pattern_errors() {
        let node = Node::new("G", "Glob");
        let result = GlobHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing config.pattern"));
    }
}
