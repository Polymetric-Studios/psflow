use crate::error::NodeError;
use crate::execute::{CancellationToken, HandlerSchema, NodeHandler, Outputs, SchemaField};
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::handlers::common::{interpolate, value_to_json};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;

/// HTTP/API call handler.
///
/// Makes HTTP requests with configurable method, URL, headers, and body.
/// Supports simple `{key}` template interpolation in URL, headers, and body
/// from the node's inputs.
///
/// ## Security
///
/// **SSRF warning**: By default, the handler allows requests to any URL
/// including internal/private networks. Set `config.allow_private: false`
/// (the default) to block requests to private IP ranges (RFC 1918,
/// link-local, loopback). Set `config.allow_private: true` to allow them.
///
/// ## Configuration
///
/// - `config.url` (required): URL template, e.g. `"https://api.example.com/items/{id}"`
/// - `config.method`: HTTP method (default: `"GET"`). Supports GET, POST, PUT, PATCH, DELETE, HEAD.
/// - `config.headers`: JSON object of header name → value templates.
/// - `config.body`: Request body template (string). Sent as-is for POST/PUT/PATCH.
/// - `config.body_json`: If true, serialize the entire inputs map as JSON body (overrides `body`).
/// - `config.timeout_ms`: Request timeout in milliseconds (default: 30000).
/// - `config.allow_private`: Allow requests to private/loopback IPs (default: false).
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
            let url_template =
                config
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

            let allow_private = config
                .get("allow_private")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let body_template = config
                .get("body")
                .and_then(|v| v.as_str())
                .map(String::from);

            // Interpolate templates
            let url = interpolate(url_template, &inputs);

            // SSRF protection: block private/loopback IPs unless explicitly allowed
            if !allow_private {
                if let Ok(parsed) = reqwest::Url::parse(&url) {
                    if let Some(host) = parsed.host_str() {
                        if is_private_host(host) {
                            return Err(NodeError::Failed {
                                source_message: None,
                                message: format!(
                                    "node '{node_id}': blocked request to private/loopback address '{host}'. \
                                     Set config.allow_private: true to allow"
                                ),
                                recoverable: false,
                            });
                        }
                    }
                }
            }

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

    fn schema(&self, name: &str) -> HandlerSchema {
        HandlerSchema::new(name, "Make an HTTP request")
            .with_config(
                SchemaField::new("url", "string")
                    .required()
                    .describe("URL template with {key} interpolation"),
            )
            .with_config(
                SchemaField::new("method", "string")
                    .describe("HTTP method")
                    .default(serde_json::json!("GET")),
            )
            .with_config(SchemaField::new("headers", "map<string,string>"))
            .with_config(SchemaField::new("body", "string"))
            .with_config(
                SchemaField::new("body_json", "boolean")
                    .describe("Serialise inputs map as JSON body")
                    .default(serde_json::json!(false)),
            )
            .with_config(
                SchemaField::new("timeout_ms", "integer").default(serde_json::json!(30_000)),
            )
            .with_config(
                SchemaField::new("allow_private", "boolean")
                    .describe("Allow requests to private/loopback IPs")
                    .default(serde_json::json!(false)),
            )
            .with_output(SchemaField::new("status", "integer"))
            .with_output(SchemaField::new("body", "string"))
            .with_output(SchemaField::new("headers", "map<string,string>"))
    }
}

/// Check if a hostname resolves to a private/loopback/link-local address.
fn is_private_host(host: &str) -> bool {
    // Check common private hostnames
    if host == "localhost" || host == "0.0.0.0" || host == "::1" || host == "[::1]" {
        return true;
    }

    // Check IP address ranges
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback()           // 127.0.0.0/8
                    || v4.is_private()      // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                    || v4.is_link_local()   // 169.254.0.0/16
                    || v4.is_unspecified() // 0.0.0.0
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()           // ::1
                    || v6.is_unspecified() // ::
            }
        };
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_url_errors() {
        let node = Node::new("H", "Http");
        let result = HttpHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing config.url"));
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
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unsupported HTTP method"));
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

    // -- SSRF protection --

    #[tokio::test]
    async fn blocks_localhost_by_default() {
        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({ "url": "http://localhost:8080/admin" });

        let result = HttpHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private/loopback"));
    }

    #[tokio::test]
    async fn blocks_private_ip_by_default() {
        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({ "url": "http://192.168.1.1/secret" });

        let result = HttpHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private/loopback"));
    }

    #[tokio::test]
    async fn blocks_metadata_endpoint() {
        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({ "url": "http://169.254.169.254/latest/meta-data/" });

        let result = HttpHandler
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn is_private_host_checks() {
        assert!(is_private_host("localhost"));
        assert!(is_private_host("127.0.0.1"));
        assert!(is_private_host("10.0.0.1"));
        assert!(is_private_host("172.16.0.1"));
        assert!(is_private_host("192.168.1.1"));
        assert!(is_private_host("169.254.169.254"));
        assert!(is_private_host("0.0.0.0"));
        assert!(is_private_host("::1"));
        assert!(!is_private_host("8.8.8.8"));
        assert!(!is_private_host("example.com"));
    }
}
