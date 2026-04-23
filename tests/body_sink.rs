//! Integration tests for `body_sink` — streaming HTTP response bodies to
//! disk. Covers:
//!
//! - Happy path: bytes on disk match the served body.
//! - Non-2xx with `fail_on_non_2xx=false` + `body_sink`: no file written,
//!   `skipped=true`, error snippet returned.
//! - Retry truncates between attempts so the final file does not contain
//!   append-accumulated content from prior retries.
//! - Path template interpolation from inputs.
//! - `create_parents=true` creates missing directories.

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

fn string_of(out: &Outputs, key: &str) -> String {
    match out.get(key).unwrap_or_else(|| panic!("missing key {key}")) {
        Value::String(s) => s.clone(),
        other => panic!("expected string at {key}, got {other:?}"),
    }
}

fn bool_of(out: &Outputs, key: &str) -> bool {
    match out.get(key).unwrap_or_else(|| panic!("missing key {key}")) {
        Value::Bool(b) => *b,
        other => panic!("expected bool at {key}, got {other:?}"),
    }
}

#[tokio::test]
async fn body_sink_writes_bytes_to_disk_on_happy_path() {
    let server = MockServer::start().await;
    let payload = b"hello streaming world";
    Mock::given(method("GET"))
        .and(path("/blob"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.to_vec()))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let out_path = tmp.path().join("downloaded.bin");

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": format!("{}/blob", server.uri()),
        "allow_private": true,
        "body_sink": {
            "file": { "path": out_path.to_str().unwrap() }
        }
    });

    let out = HttpHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("happy path should succeed");

    assert_eq!(i64_of(&out, "status"), 200);
    assert_eq!(i64_of(&out, "bytes_written"), payload.len() as i64);
    assert_eq!(string_of(&out, "path"), out_path.to_string_lossy());
    // No body field — we streamed.
    assert!(!out.contains_key("body"));

    let on_disk = std::fs::read(&out_path).unwrap();
    assert_eq!(on_disk, payload);
}

#[tokio::test]
async fn body_sink_non_2xx_passthrough_skips_write() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/gone"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not here, sorry"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let out_path = tmp.path().join("should-not-exist.bin");

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": format!("{}/gone", server.uri()),
        "allow_private": true,
        // fail_on_non_2xx is false by default.
        "body_sink": {
            "file": { "path": out_path.to_str().unwrap() }
        }
    });

    let out = HttpHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("passthrough non-2xx should not fail the node");

    assert_eq!(i64_of(&out, "status"), 404);
    assert_eq!(i64_of(&out, "bytes_written"), 0);
    assert!(bool_of(&out, "skipped"));
    assert!(string_of(&out, "error_body_snippet").contains("not here"));
    assert!(
        !out_path.exists(),
        "file should not exist on passthrough non-2xx: {out_path:?}"
    );
}

#[tokio::test]
async fn body_sink_fail_on_non_2xx_errors_before_write() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("server upset"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let out_path = tmp.path().join("never-written.bin");

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": server.uri(),
        "allow_private": true,
        "fail_on_non_2xx": true,
        "body_sink": {
            "file": { "path": out_path.to_str().unwrap() }
        }
    });

    let result = HttpHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await;
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("500"), "err: {err}");
    assert!(err.contains("not written"), "err: {err}");
    assert!(!out_path.exists(), "file should not be created");
}

#[tokio::test]
async fn body_sink_retry_truncates_between_attempts() {
    let server = MockServer::start().await;
    // First response: 503 with a distinctive body that MUST NOT end up on disk.
    Mock::given(method("GET"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(503).set_body_string("STALE-ERROR-BODY"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // Second response: 200 with the real payload.
    Mock::given(method("GET"))
        .and(path("/flaky"))
        .respond_with(ResponseTemplate::new(200).set_body_string("FRESH-OK-BODY"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let out_path = tmp.path().join("retried.bin");

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": format!("{}/flaky", server.uri()),
        "allow_private": true,
        "retry": {
            "max_attempts": 3,
            "backoff": "fixed",
            "delay_ms": 1,
            "retry_on": ["5xx"]
        },
        "body_sink": {
            "file": { "path": out_path.to_str().unwrap() }
        }
    });

    let out = HttpHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("retry path should succeed");

    assert_eq!(i64_of(&out, "status"), 200);
    let on_disk = std::fs::read_to_string(&out_path).unwrap();
    // Exact match — no residue of the 503 body. Truncate-on-open is the
    // guarantee here; this fails loudly if we regress to an append path.
    assert_eq!(on_disk, "FRESH-OK-BODY");
    assert_eq!(i64_of(&out, "bytes_written"), "FRESH-OK-BODY".len() as i64);
}

#[tokio::test]
async fn body_sink_path_template_interpolates_from_inputs() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("payload-v7"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    // Template uses an input key.
    let template = format!("{}/{{name}}.bin", tmp.path().display());

    let mut inputs = Outputs::new();
    inputs.insert("name".into(), Value::String("artifact".into()));

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": server.uri(),
        "allow_private": true,
        "body_sink": {
            "file": { "path": template }
        }
    });

    let out = HttpHandler::stateless()
        .execute(&node, inputs, CancellationToken::new())
        .await
        .expect("templated path should resolve");

    let expected = tmp.path().join("artifact.bin");
    assert_eq!(string_of(&out, "path"), expected.to_string_lossy());
    let contents = std::fs::read_to_string(&expected).unwrap();
    assert_eq!(contents, "payload-v7");
}

#[tokio::test]
async fn body_sink_create_parents_creates_missing_dirs() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("deep"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    // Nested path that does not exist yet.
    let nested = tmp.path().join("a").join("b").join("c").join("file.bin");
    assert!(!nested.parent().unwrap().exists());

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": server.uri(),
        "allow_private": true,
        "body_sink": {
            "file": {
                "path": nested.to_str().unwrap(),
                "create_parents": true
            }
        }
    });

    let out = HttpHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await
        .expect("nested path with create_parents should succeed");
    assert_eq!(i64_of(&out, "status"), 200);
    assert!(nested.exists());
    assert_eq!(std::fs::read_to_string(&nested).unwrap(), "deep");
}

#[tokio::test]
async fn body_sink_create_parents_false_fails_on_missing_dir() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("nope"))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let nested = tmp.path().join("does").join("not").join("exist.bin");

    let mut node = Node::new("H", "Http");
    node.config = serde_json::json!({
        "url": server.uri(),
        "allow_private": true,
        "body_sink": {
            "file": {
                "path": nested.to_str().unwrap(),
                "create_parents": false
            }
        }
    });

    let result = HttpHandler::stateless()
        .execute(&node, Outputs::new(), CancellationToken::new())
        .await;
    assert!(result.is_err());
    assert!(!nested.exists());
}
