//! Integration tests for HTTP response JSON Schema validation.
//!
//! Exercises end-to-end request/response against a wiremock stub with
//! both `fail` and `passthrough` failure modes, plus schema-from-file
//! with `{key}` template interpolation of the path.

use psflow::execute::{CancellationToken, NodeHandler, Outputs};
use psflow::graph::node::Node;
use psflow::handlers::HttpHandler;
use psflow::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn i64_of(out: &Outputs, key: &str) -> i64 {
    match out.get(key).unwrap_or_else(|| panic!("missing key {key}")) {
        Value::I64(n) => *n,
        other => panic!("expected i64 at {key}, got {other:?}"),
    }
}

#[tokio::test]
async fn fail_mode_succeeds_on_valid_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/widgets/1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(r#"{"id": 1, "name": "widget", "qty": 7}"#),
        )
        .mount(&server)
        .await;

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": format!("{}/widgets/1", server.uri()),
        "allow_private": true,
        "validation": {
            "inline": {
                "type": "object",
                "required": ["id", "name"],
                "properties": {
                    "id": { "type": "integer" },
                    "name": { "type": "string" },
                    "qty": { "type": "integer", "minimum": 0 }
                }
            }
        }
    });
    let out = HttpHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("valid body should succeed");
    assert_eq!(i64_of(&out, "status"), 200);
    match out.get("validation_ok").expect("validation_ok present") {
        Value::Bool(b) => assert!(*b),
        other => panic!("expected bool, got {other:?}"),
    }
}

#[tokio::test]
async fn fail_mode_errors_on_invalid_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/widgets/2"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"name": 99}"#))
        .mount(&server)
        .await;

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": format!("{}/widgets/2", server.uri()),
        "allow_private": true,
        "validation": {
            "inline": {
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "integer" },
                    "name": { "type": "string" }
                }
            }
        }
    });
    let err = HttpHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect_err("schema mismatch should fail node");
    let msg = err.to_string();
    assert!(msg.contains("schema validation failed"), "msg: {msg}");
}

#[tokio::test]
async fn passthrough_mode_attaches_validation_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/widgets/3"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"name": "no-id"}"#))
        .mount(&server)
        .await;

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": format!("{}/widgets/3", server.uri()),
        "allow_private": true,
        "validation": {
            "inline": {
                "type": "object",
                "required": ["id"]
            },
            "on_failure": "passthrough"
        }
    });
    let out = HttpHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("passthrough mode must not fail node");
    assert_eq!(i64_of(&out, "status"), 200);
    match out.get("validation_ok").unwrap() {
        Value::Bool(b) => assert!(!*b),
        other => panic!("expected bool, got {other:?}"),
    }
    // validation_error carries structured failure info.
    let err_field = out
        .get("validation_error")
        .expect("validation_error present in passthrough");
    match err_field {
        Value::Vec(items) => {
            assert!(!items.is_empty(), "expected at least one failure entry");
            // Each entry should be a map with instance_path / keyword keys.
            match &items[0] {
                Value::Map(m) => {
                    assert!(m.contains_key("keyword"));
                    assert!(m.contains_key("instance_path"));
                }
                other => panic!("expected map entry, got {other:?}"),
            }
        }
        other => panic!("expected vec of failures, got {other:?}"),
    }
}

#[tokio::test]
async fn schema_loaded_from_file_with_template_interpolated_path() {
    // Write a schema to a temp file, point `validation.file` at a
    // template that is resolved from node inputs.
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let schema = serde_json::json!({
        "type": "object",
        "required": ["ok"],
        "properties": { "ok": { "type": "boolean" } }
    });
    std::fs::write(tmp.path(), serde_json::to_string(&schema).unwrap()).unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok": true}"#))
        .mount(&server)
        .await;

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": server.uri(),
        "allow_private": true,
        "validation": {
            "file": "{schema_path}"
        }
    });

    let mut inputs = Outputs::new();
    inputs.insert(
        "schema_path".into(),
        Value::String(tmp.path().to_string_lossy().into_owned()),
    );

    let out = HttpHandler::stateless()
        .execute(&node, inputs, CancellationToken::new())
        .await
        .expect("file-based schema should validate");
    match out.get("validation_ok").unwrap() {
        Value::Bool(b) => assert!(*b),
        other => panic!("expected bool, got {other:?}"),
    }
}
