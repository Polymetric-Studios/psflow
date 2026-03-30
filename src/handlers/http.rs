use crate::error::NodeError;
use crate::execute::{CancellationToken, NodeHandler, Outputs};
use crate::graph::node::Node;
use crate::graph::types::Value;
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;

/// HTTP/API call handler.
///
/// Makes HTTP requests with configurable method, URL, headers, and body.
/// Supports simple `{key}` template interpolation in URL, headers, and body
/// from the node's inputs.
///
/// ## Configuration
///
/// - `config.url` (required): URL template, e.g. `"https://api.example.com/items/{id}"`
/// - `config.method`: HTTP method (default: `"GET"`). Supports GET, POST, PUT, PATCH, DELETE, HEAD.
/// - `config.headers`: JSON object of header name → value templates.
/// - `config.body`: Request body template (string). Sent as-is for POST/PUT/PATCH.
/// - `config.body_json`: If true, serialize the entire inputs map as JSON body (overrides `body`).
/// - `config.timeout_ms`: Request timeout in milliseconds (default: 30000).
///
/// ## Outputs
///
/// - `status`: HTTP status code (i64)
/// - `body`: Response body as string
/// - `headers`: Response headers as Map<String, String>
pub struct HttpHandler;

impl NodeHandler for HttpHandler {
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
                    reason: "cancelled before HTTP request".into(),
                });
            }

            // Parse config
            let url_template = config
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': missing config.url"),
                    recoverable: false,
                })?;

            let method = config
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_uppercase();

            let timeout_ms = config
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(30_000);

            let header_templates: HashMap<String, String> = config
                .get("headers")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();

            let body_json = config
                .get("body_json")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let body_template = config.get("body").and_then(|v| v.as_str()).map(String::from);

            // Interpolate templates
            let url = interpolate(url_template, &inputs);

            // Build request
            let client = reqwest::Client::new();
            let mut request = match method.as_str() {
                "GET" => client.get(&url),
                "POST" => client.post(&url),
                "PUT" => client.put(&url),
                "PATCH" => client.patch(&url),
                "DELETE" => client.delete(&url),
                "HEAD" => client.head(&url),
                other => {
                    return Err(NodeError::Failed {
                        source_message: None,
                        message: format!("node '{node_id}': unsupported HTTP method '{other}'"),
                        recoverable: false,
                    })
                }
            };

            request = request.timeout(std::time::Duration::from_millis(timeout_ms));

            // Headers
            for (name, value_template) in &header_templates {
                let value = interpolate(value_template, &inputs);
                request = request.header(name.as_str(), value);
            }

            // Body
            if body_json {
                let json_body: serde_json::Value = inputs
                    .iter()
                    .map(|(k, v)| (k.clone(), value_to_json(v)))
                    .collect::<serde_json::Map<String, serde_json::Value>>()
                    .into();
                request = request
                    .header("content-type", "application/json")
                    .body(json_body.to_string());
            } else if let Some(tmpl) = &body_template {
                let body = interpolate(tmpl, &inputs);
                request = request.body(body);
            }

            // Execute with cancellation
            let response = tokio::select! {
                result = request.send() => {
                    result.map_err(|e| NodeError::Failed {
                        source_message: Some(e.to_string()),
                        message: format!("node '{node_id}': HTTP request failed: {e}"),
                        recoverable: e.is_timeout(),
                    })?
                }
                _ = cancel.cancelled() => {
                    return Err(NodeError::Cancelled {
                        reason: "cancelled during HTTP request".into(),
                    });
                }
            };

            // Extract response
            let status = response.status().as_u16() as i64;

            let resp_headers: BTreeMap<String, Value> = response
                .headers()
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        Value::String(v.to_str().unwrap_or("").to_string()),
                    )
                })
                .collect();

            let body = response.text().await.map_err(|e| NodeError::Failed {
                source_message: Some(e.to_string()),
                message: format!("node '{node_id}': failed to read response body: {e}"),
                recoverable: false,
            })?;

            let mut outputs = Outputs::new();
            outputs.insert("status".into(), Value::I64(status));
            outputs.insert("body".into(), Value::String(body));
            outputs.insert("headers".into(), Value::Map(resp_headers));

            Ok(outputs)
        })
    }
}

/// Simple `{key}` template interpolation from inputs.
fn interpolate(template: &str, inputs: &Outputs) -> String {
    let mut result = template.to_string();
    for (key, value) in inputs {
        let placeholder = format!("{{{key}}}");
        let replacement = match value {
            Value::String(s) => s.clone(),
            Value::I64(n) => n.to_string(),
            Value::F32(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => continue,
        };
        result = result.replace(&placeholder, &replacement);
    }
    result
}

/// Convert a graph Value to a serde_json::Value.
fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::I64(n) => serde_json::json!(*n),
        Value::F32(f) => serde_json::json!(*f),
        Value::Vec(items) => {
            serde_json::Value::Array(items.iter().map(value_to_json).collect())
        }
        Value::Map(map) => {
            let obj: serde_json::Map<String, serde_json::Value> =
                map.iter().map(|(k, v)| (k.clone(), value_to_json(v))).collect();
            serde_json::Value::Object(obj)
        }
        Value::Domain { data, .. } => data.clone(),
        Value::Null => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolate_simple() {
        let mut inputs = Outputs::new();
        inputs.insert("id".into(), Value::I64(42));
        inputs.insert("name".into(), Value::String("test".into()));

        let result = interpolate("https://api.example.com/{name}/{id}", &inputs);
        assert_eq!(result, "https://api.example.com/test/42");
    }

    #[test]
    fn interpolate_no_placeholders() {
        let result = interpolate("https://example.com", &Outputs::new());
        assert_eq!(result, "https://example.com");
    }

    #[test]
    fn interpolate_missing_key_left_as_is() {
        let result = interpolate("https://example.com/{missing}", &Outputs::new());
        assert_eq!(result, "https://example.com/{missing}");
    }

    #[test]
    fn value_to_json_conversions() {
        assert_eq!(
            value_to_json(&Value::String("hello".into())),
            serde_json::json!("hello")
        );
        assert_eq!(value_to_json(&Value::I64(42)), serde_json::json!(42));
        assert_eq!(value_to_json(&Value::Bool(true)), serde_json::json!(true));
        assert_eq!(value_to_json(&Value::Null), serde_json::Value::Null);
    }

    #[tokio::test]
    async fn missing_url_errors() {
        let node = Node::new("H", "Http");
        let result = HttpHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing config.url"));
    }

    #[tokio::test]
    async fn unsupported_method_errors() {
        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": "https://example.com",
            "method": "CONNECT"
        });
        let result = HttpHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unsupported HTTP method"));
    }

    #[tokio::test]
    async fn cancellation_before_request() {
        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({ "url": "https://example.com" });

        let token = CancellationToken::new();
        token.cancel();

        let result = HttpHandler.execute(&node, Outputs::new(), token).await;
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }
}
