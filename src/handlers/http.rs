use crate::auth::{AuthApplyCtx, AuthError, AuthStrategy};
use crate::error::NodeError;
use crate::execute::{
    CancellationToken, ExecutionContext, HandlerSchema, NodeHandler, Outputs, SchemaField,
};
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::handlers::common::{interpolate, value_to_json};
use crate::template::{PromptTemplateResolver, TemplateResolver};
use crate::validation::{CompiledValidator, FailureMode, ValidationConfig, ValidationOutcome};
use futures::StreamExt;
use reqwest::multipart::{Form, Part};
use reqwest::redirect::Policy as RedirectPolicy;
use reqwest::{RequestBuilder, Response};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::{Path as StdPath, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// First-N bytes of an error response body we surface on `body_sink`
/// short-circuit. Cap prevents OOM on multi-MB error pages.
const ERROR_BODY_SNIPPET_CAP: usize = 4096;

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
/// - `config.multipart`: Multipart/form-data body. Object with:
///     - `fields`: map of text field name → string-template value
///     - `files`: array of `{name, path?, bytes?, filename?, content_type?}` parts
/// - `config.timeout_ms`: Request timeout in milliseconds (default: 30000).
/// - `config.allow_private`: Allow requests to private/loopback IPs (default: false).
/// - `config.auth`: Name of a graph-scoped auth strategy (from
///   `GraphMetadata.auth`). Looked up at execution time.
/// - `config.redirect`: Redirect policy. `"none"`, `{ "limited": n }`, or `"default"` (reqwest default of 10).
/// - `config.retry`: HTTP-scoped retry policy. `{ max_attempts, backoff: "fixed"|"exponential",
///   delay_ms, multiplier, max_delay_ms, retry_on: [...] }`. `retry_on` accepts status codes,
///   the string `"5xx"`, and/or `"connection_error"`.
/// - `config.fail_on_non_2xx`: If true, non-2xx responses fail the node (body + status carried
///   in the error message for diagnosis). Default false preserves status/body passthrough.
/// - `config.validation`: Declarative JSON Schema validation of the response body. Shape:
///   `{ "inline": <schema> } | { "file": "<path-template>" }` with optional
///   `"on_failure": "fail" | "passthrough"` (default `"fail"`). Runs AFTER `fail_on_non_2xx`.
///   Non-JSON bodies fail (or attach a `validation_error` in passthrough mode).
///   **Mutually exclusive with `config.body_sink`** — validation needs a buffered body.
/// - `config.body_sink`: Stream the response body to disk instead of buffering. Shape:
///   `{ "file": { "path": "<path-template>", "overwrite"?: "always"|"if_missing"|"never",
///   "create_parents"?: bool } }`. Defaults: `overwrite=always`, `create_parents=true`.
///   When set, outputs `path` + `bytes_written` instead of `body`. On non-2xx responses
///   the write is skipped (see `fail_on_non_2xx`).
///
/// ## Outputs
///
/// Default (no `body_sink`):
/// - `status`: HTTP status code (i64)
/// - `body`: Response body as string
/// - `headers`: Response headers as Map<String, String>
///
/// With `body_sink` set and write succeeded:
/// - `status`, `headers` (as above)
/// - `path`: final on-disk path (String)
/// - `bytes_written`: number of bytes streamed (i64)
///
/// With `body_sink` set and write skipped (IfMissing hit, or non-2xx passthrough):
/// - `status`, `headers`, `path`, `bytes_written=0`
/// - `skipped: true`
/// - `error_body_snippet` (non-2xx only): first 4KB of the error body
pub struct HttpHandler {
    /// Execution context for auth registry, secret resolver, and auth state
    /// lookups. `None` means the handler operates without auth — requests
    /// that reference `config.auth` will fail.
    exec_ctx: Option<Arc<ExecutionContext>>,
    /// Template resolver used by auth strategy param interpolation.
    template: Arc<dyn TemplateResolver>,
    /// Compiled validators, keyed by the raw JSON text of the
    /// `config.validation` block. Compiled lazily on first use per node and
    /// retained for the lifetime of the handler so we do not re-parse the
    /// schema per request.
    validators: Arc<Mutex<HashMap<String, CompiledValidator>>>,
}

impl HttpHandler {
    /// Stateless HTTP handler — no auth support. Useful for tests and
    /// handlers in graphs that never reference auth strategies.
    pub fn stateless() -> Self {
        Self {
            exec_ctx: None,
            template: Arc::new(PromptTemplateResolver),
            validators: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// HTTP handler wired up to an execution context. Required for
    /// `config.auth` to resolve against the graph's auth registry.
    pub fn new(ctx: Arc<ExecutionContext>) -> Self {
        Self {
            exec_ctx: Some(ctx),
            template: Arc::new(PromptTemplateResolver),
            validators: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_template_resolver(mut self, resolver: Arc<dyn TemplateResolver>) -> Self {
        self.template = resolver;
        self
    }
}

impl Default for HttpHandler {
    fn default() -> Self {
        Self::stateless()
    }
}

/// Result of running validation against an HTTP response body. Captures
/// both schema-level failures and the "body is not JSON" case so the
/// per-mode handling (fail vs. passthrough) can treat them uniformly.
enum ValidationReport {
    Valid,
    Invalid {
        errors: Vec<crate::validation::ValidationFailure>,
    },
    NotJson {
        reason: String,
    },
}

fn run_validation(validator: &CompiledValidator, body: &str) -> (FailureMode, ValidationReport) {
    let mode = validator.failure_mode();
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            return (
                mode,
                ValidationReport::NotJson {
                    reason: e.to_string(),
                },
            );
        }
    };
    match validator.validate(&parsed) {
        ValidationOutcome::Valid => (mode, ValidationReport::Valid),
        ValidationOutcome::Invalid { errors } => (mode, ValidationReport::Invalid { errors }),
    }
}

/// Translate a validation report into either a node failure (in `fail`
/// mode) or injected `validation_ok` / `validation_error` output fields
/// (in `passthrough` mode).
fn apply_validation_report(
    node_id: &str,
    mode: FailureMode,
    report: ValidationReport,
    outputs: &mut Outputs,
) -> Result<(), NodeError> {
    match (mode, report) {
        (_, ValidationReport::Valid) => {
            outputs.insert("validation_ok".into(), Value::Bool(true));
            Ok(())
        }
        (FailureMode::Fail, ValidationReport::Invalid { errors }) => {
            let detail = serde_json::to_string(&errors).unwrap_or_else(|_| "[]".into());
            Err(NodeError::Failed {
                source_message: Some(detail.clone()),
                message: format!("node '{node_id}': response schema validation failed: {detail}"),
                recoverable: false,
            })
        }
        (FailureMode::Fail, ValidationReport::NotJson { reason }) => Err(NodeError::Failed {
            source_message: Some(reason.clone()),
            message: format!("node '{node_id}': response body not JSON: {reason}"),
            recoverable: false,
        }),
        (FailureMode::Passthrough, ValidationReport::Invalid { errors }) => {
            let detail_json = serde_json::to_value(&errors).unwrap_or(serde_json::Value::Null);
            outputs.insert("validation_ok".into(), Value::Bool(false));
            outputs.insert("validation_error".into(), json_to_value(&detail_json));
            Ok(())
        }
        (FailureMode::Passthrough, ValidationReport::NotJson { reason }) => {
            outputs.insert("validation_ok".into(), Value::Bool(false));
            outputs.insert(
                "validation_error".into(),
                json_to_value(&serde_json::json!({
                    "kind": "not_json",
                    "reason": reason,
                })),
            );
            Ok(())
        }
    }
}

/// Shallow conversion from `serde_json::Value` to graph `Value`. Used for
/// the `validation_error` output field in passthrough mode.
fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::I64(i)
            } else if let Some(f) = n.as_f64() {
                Value::F32(f as f32)
            } else {
                Value::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(arr) => Value::Vec(arr.iter().map(json_to_value).collect()),
        serde_json::Value::Object(obj) => Value::Map(
            obj.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

/// Get-or-compile a validator for the given `validation` config block,
/// caching it in `cache` keyed by the raw JSON text so repeated executions
/// reuse the compiled schema. File-backed schemas are loaded from disk on
/// first miss; the `path_template` is `{key}`-interpolated against
/// `inputs`.
fn get_or_compile_validator(
    cache: &Mutex<HashMap<String, CompiledValidator>>,
    validation_json: &serde_json::Value,
    inputs: &Outputs,
) -> Result<CompiledValidator, String> {
    let key = validation_json.to_string();
    if let Some(v) = cache.lock().unwrap().get(&key).cloned() {
        return Ok(v);
    }
    let cfg = ValidationConfig::from_json(validation_json).map_err(|e| e.to_string())?;
    let resolver = |tmpl: &str| std::path::PathBuf::from(interpolate(tmpl, inputs));
    let validator = CompiledValidator::from_config(&cfg, resolver).map_err(|e| e.to_string())?;
    cache.lock().unwrap().insert(key, validator.clone());
    Ok(validator)
}

/// Response body sink — controls whether the body is buffered into the
/// `body` output (the default) or streamed to disk.
#[derive(Debug, Clone)]
enum BodySinkCfg {
    /// Stream response bytes directly to a file on disk. Output replaces
    /// the `body` field with `path` + `bytes_written`.
    File {
        path_template: String,
        overwrite: Overwrite,
        create_parents: bool,
    },
}

/// Overwrite policy for `body_sink = file`.
///
/// - `Always` (default for templated downloads): overwrite any existing file.
/// - `IfMissing`: skip the request entirely if the target already exists —
///   short-circuit before the network hop.
/// - `Never`: if the target already exists, fail the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overwrite {
    Always,
    IfMissing,
    Never,
}

impl Overwrite {
    fn parse(v: Option<&serde_json::Value>) -> Result<Self, String> {
        let Some(v) = v else {
            return Ok(Self::Always);
        };
        if let Some(s) = v.as_str() {
            return match s {
                "always" => Ok(Self::Always),
                "if_missing" | "if-missing" | "ifMissing" => Ok(Self::IfMissing),
                "never" => Ok(Self::Never),
                other => Err(format!(
                    "body_sink.overwrite: expected \"always\" | \"if_missing\" | \"never\", got '{other}'"
                )),
            };
        }
        if let Some(b) = v.as_bool() {
            return Ok(if b { Self::Always } else { Self::Never });
        }
        Err("body_sink.overwrite must be a string or a bool".into())
    }
}

impl BodySinkCfg {
    /// Parse a `body_sink` config node. Returns `Ok(None)` when the config
    /// is absent (default in-memory behavior).
    fn from_config(v: Option<&serde_json::Value>) -> Result<Option<Self>, String> {
        let Some(v) = v else {
            return Ok(None);
        };
        let obj = v
            .as_object()
            .ok_or_else(|| "body_sink must be an object".to_string())?;
        // Only `file` is supported today — future sinks (e.g. stdout, null)
        // can slot in as sibling keys.
        let file = obj.get("file").ok_or_else(|| {
            "body_sink requires a `file` object (e.g. { \"file\": { \"path\": ... } })".to_string()
        })?;
        let fobj = file
            .as_object()
            .ok_or_else(|| "body_sink.file must be an object".to_string())?;
        let path_template = fobj
            .get("path")
            .and_then(|x| x.as_str())
            .ok_or_else(|| "body_sink.file.path is required".to_string())?
            .to_string();
        let overwrite = Overwrite::parse(fobj.get("overwrite"))?;
        let create_parents = fobj
            .get("create_parents")
            .and_then(|x| x.as_bool())
            .unwrap_or(true);
        Ok(Some(Self::File {
            path_template,
            overwrite,
            create_parents,
        }))
    }
}

/// Redirect policy config surface.
#[derive(Debug, Clone)]
enum RedirectCfg {
    /// reqwest default (currently 10).
    Default,
    /// No redirects.
    None,
    /// Cap at N redirects.
    Limited(usize),
}

impl RedirectCfg {
    fn from_config(v: Option<&serde_json::Value>) -> Result<Self, String> {
        let Some(v) = v else {
            return Ok(Self::Default);
        };
        if let Some(s) = v.as_str() {
            return match s {
                "none" => Ok(Self::None),
                "default" => Ok(Self::Default),
                other => Err(format!("invalid redirect policy string: '{other}'")),
            };
        }
        if let Some(obj) = v.as_object() {
            if let Some(n) = obj.get("limited").and_then(|n| n.as_u64()) {
                return Ok(Self::Limited(n as usize));
            }
            if let Some(n) = obj.get("max").and_then(|n| n.as_u64()) {
                return Ok(Self::Limited(n as usize));
            }
        }
        Err("redirect must be \"none\", \"default\", or { \"limited\": N }".into())
    }

    fn into_policy(self) -> RedirectPolicy {
        match self {
            RedirectCfg::Default => RedirectPolicy::default(),
            RedirectCfg::None => RedirectPolicy::none(),
            RedirectCfg::Limited(n) => RedirectPolicy::limited(n),
        }
    }
}

/// HTTP-scoped retry backoff.
///
/// Parallel to `execute::retry::BackoffStrategy` but lives here so the
/// HTTP handler can inspect response status before deciding whether to
/// retry — node-level retry can't see the response.
#[derive(Debug, Clone)]
enum HttpBackoff {
    Fixed {
        delay_ms: u64,
    },
    Exponential {
        initial_delay_ms: u64,
        multiplier: f64,
        max_delay_ms: u64,
    },
}

impl HttpBackoff {
    fn delay_for(&self, attempt: u32) -> Duration {
        let ms = match self {
            HttpBackoff::Fixed { delay_ms } => *delay_ms,
            HttpBackoff::Exponential {
                initial_delay_ms,
                multiplier,
                max_delay_ms,
            } => {
                let clamped = attempt.min(63);
                let d = (*initial_delay_ms as f64) * multiplier.powi(clamped as i32);
                let d = if d.is_finite() {
                    d as u64
                } else {
                    *max_delay_ms
                };
                d.min(*max_delay_ms)
            }
        };
        Duration::from_millis(ms)
    }
}

/// What triggers a retry.
#[derive(Debug, Clone, Default)]
struct RetryOn {
    /// Specific status codes (e.g. 429).
    statuses: HashSet<u16>,
    /// Any 5xx status.
    any_5xx: bool,
    /// Connection-level / transport errors.
    connection_error: bool,
}

impl RetryOn {
    fn parse(v: Option<&serde_json::Value>) -> Result<Self, String> {
        let mut out = RetryOn::default();
        let Some(v) = v else {
            // Default: retry on connection errors + 5xx.
            out.any_5xx = true;
            out.connection_error = true;
            return Ok(out);
        };
        let items: Vec<&serde_json::Value> = match v {
            serde_json::Value::Array(arr) => arr.iter().collect(),
            single => vec![single],
        };
        for item in items {
            match item {
                serde_json::Value::String(s) => match s.as_str() {
                    "5xx" => out.any_5xx = true,
                    "connection_error" => out.connection_error = true,
                    other => {
                        // allow e.g. "503" as a string
                        if let Ok(code) = other.parse::<u16>() {
                            out.statuses.insert(code);
                        } else {
                            return Err(format!("unknown retry_on token: '{other}'"));
                        }
                    }
                },
                serde_json::Value::Number(n) => {
                    if let Some(c) = n.as_u64() {
                        out.statuses.insert(c as u16);
                    }
                }
                other => return Err(format!("invalid retry_on entry: {other}")),
            }
        }
        Ok(out)
    }

    fn matches_status(&self, status: u16) -> bool {
        if self.statuses.contains(&status) {
            return true;
        }
        if self.any_5xx && (500..600).contains(&status) {
            return true;
        }
        false
    }

    fn matches_conn_err(&self) -> bool {
        self.connection_error
    }
}

#[derive(Debug, Clone)]
struct HttpRetryConfig {
    max_attempts: u32,
    backoff: HttpBackoff,
    retry_on: RetryOn,
}

impl HttpRetryConfig {
    fn from_config(v: Option<&serde_json::Value>) -> Result<Option<Self>, String> {
        let Some(v) = v else {
            return Ok(None);
        };
        let max_attempts = v
            .get("max_attempts")
            .and_then(|x| x.as_u64())
            .unwrap_or(1)
            .min(100) as u32;
        if max_attempts <= 1 {
            return Ok(None);
        }
        let backoff_type = v.get("backoff").and_then(|x| x.as_str()).unwrap_or("fixed");
        let delay_ms = v.get("delay_ms").and_then(|x| x.as_u64()).unwrap_or(100);
        let backoff = match backoff_type {
            "exponential" => {
                let multiplier = v.get("multiplier").and_then(|x| x.as_f64()).unwrap_or(2.0);
                let max_delay_ms = v
                    .get("max_delay_ms")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(60_000);
                HttpBackoff::Exponential {
                    initial_delay_ms: delay_ms,
                    multiplier,
                    max_delay_ms,
                }
            }
            _ => HttpBackoff::Fixed { delay_ms },
        };
        let retry_on = RetryOn::parse(v.get("retry_on"))?;
        Ok(Some(Self {
            max_attempts,
            backoff,
            retry_on,
        }))
    }
}

/// A multipart file part, as declared in config.
enum MultipartFile {
    /// Read contents from `path`. Template-interpolated.
    Path {
        name: String,
        path: String,
        filename: Option<String>,
        content_type: Option<String>,
    },
    /// Inline bytes. `bytes_base64` is base64 or `bytes` is a string.
    Inline {
        name: String,
        bytes: Vec<u8>,
        filename: Option<String>,
        content_type: Option<String>,
    },
}

struct MultipartCfg {
    fields: Vec<(String, String)>,
    files: Vec<MultipartFile>,
}

impl MultipartCfg {
    fn from_config(v: &serde_json::Value) -> Result<Self, String> {
        let obj = v
            .as_object()
            .ok_or_else(|| "multipart must be an object".to_string())?;
        let mut fields = Vec::new();
        if let Some(fmap) = obj.get("fields").and_then(|x| x.as_object()) {
            for (k, v) in fmap {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("multipart field '{k}' must be a string"))?;
                fields.push((k.clone(), s.to_string()));
            }
        }
        let mut files = Vec::new();
        if let Some(farr) = obj.get("files").and_then(|x| x.as_array()) {
            for (i, entry) in farr.iter().enumerate() {
                let e = entry
                    .as_object()
                    .ok_or_else(|| format!("multipart.files[{i}] must be an object"))?;
                let name = e
                    .get("name")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| format!("multipart.files[{i}].name required"))?
                    .to_string();
                let filename = e.get("filename").and_then(|x| x.as_str()).map(String::from);
                let content_type = e
                    .get("content_type")
                    .and_then(|x| x.as_str())
                    .map(String::from);
                if let Some(path) = e.get("path").and_then(|x| x.as_str()) {
                    files.push(MultipartFile::Path {
                        name,
                        path: path.to_string(),
                        filename,
                        content_type,
                    });
                } else if let Some(bytes_str) = e.get("bytes").and_then(|x| x.as_str()) {
                    files.push(MultipartFile::Inline {
                        name,
                        bytes: bytes_str.as_bytes().to_vec(),
                        filename,
                        content_type,
                    });
                } else if let Some(arr) = e.get("bytes").and_then(|x| x.as_array()) {
                    let mut buf = Vec::with_capacity(arr.len());
                    for b in arr {
                        let n = b.as_u64().ok_or_else(|| {
                            format!("multipart.files[{i}].bytes array must be u8 values")
                        })?;
                        if n > 255 {
                            return Err(format!(
                                "multipart.files[{i}].bytes: value {n} out of u8 range"
                            ));
                        }
                        buf.push(n as u8);
                    }
                    files.push(MultipartFile::Inline {
                        name,
                        bytes: buf,
                        filename,
                        content_type,
                    });
                } else {
                    return Err(format!(
                        "multipart.files[{i}] must have one of `path` or `bytes`"
                    ));
                }
            }
        }
        Ok(MultipartCfg { fields, files })
    }
}

/// Resolved auth binding: strategy name + strategy + exec ctx.
type AuthBinding = (String, Arc<dyn AuthStrategy>, Arc<ExecutionContext>);

impl NodeHandler for HttpHandler {
    fn execute(
        &self,
        node: &Node,
        inputs: Outputs,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<Outputs, NodeError>> + Send>> {
        let config = node.config.clone();
        let node_id = node.id.0.clone();
        let exec_ctx = self.exec_ctx.clone();
        let template = self.template.clone();
        let validators = self.validators.clone();

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

            let multipart_cfg = if let Some(v) = config.get("multipart") {
                Some(MultipartCfg::from_config(v).map_err(|e| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': invalid multipart config: {e}"),
                    recoverable: false,
                })?)
            } else {
                None
            };

            let auth_name = config
                .get("auth")
                .and_then(|v| v.as_str())
                .map(String::from);

            let fail_on_non_2xx = config
                .get("fail_on_non_2xx")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let redirect_cfg = RedirectCfg::from_config(config.get("redirect")).map_err(|e| {
                NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': {e}"),
                    recoverable: false,
                }
            })?;

            let retry_cfg = HttpRetryConfig::from_config(config.get("retry")).map_err(|e| {
                NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': invalid retry config: {e}"),
                    recoverable: false,
                }
            })?;

            let body_sink_cfg = BodySinkCfg::from_config(config.get("body_sink")).map_err(|e| {
                NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': invalid body_sink config: {e}"),
                    recoverable: false,
                }
            })?;

            // Streaming response to disk is incompatible with declarative
            // response-body schema validation — the whole point of `body_sink`
            // is to avoid buffering the body, which validation requires.
            // Reject at config-load time rather than silently degrading.
            if body_sink_cfg.is_some() && config.get("validation").is_some() {
                return Err(NodeError::Failed {
                    source_message: None,
                    message: format!(
                        "node '{node_id}': `body_sink` and `validation` are mutually exclusive — \
                         validation needs the parsed body, body_sink streams it to disk without \
                         buffering"
                    ),
                    recoverable: false,
                });
            }

            // Interpolate templates
            let url = interpolate(url_template, &inputs);

            // Resolve the body_sink path up front (template depends on inputs,
            // not on response). Needed for overwrite-policy short-circuit
            // before we do any network work.
            let body_sink_resolved: Option<(PathBuf, Overwrite, bool)> = match &body_sink_cfg {
                Some(BodySinkCfg::File {
                    path_template,
                    overwrite,
                    create_parents,
                }) => {
                    let resolved = PathBuf::from(interpolate(path_template, &inputs));
                    Some((resolved, *overwrite, *create_parents))
                }
                None => None,
            };

            if let Some((path, overwrite, _)) = body_sink_resolved.as_ref() {
                let exists = tokio::fs::metadata(path).await.is_ok();
                match overwrite {
                    Overwrite::IfMissing if exists => {
                        // Short-circuit — skip the request entirely.
                        let mut outputs = Outputs::new();
                        outputs.insert("status".into(), Value::I64(0));
                        outputs.insert(
                            "path".into(),
                            Value::String(path.to_string_lossy().into_owned()),
                        );
                        outputs.insert("bytes_written".into(), Value::I64(0));
                        outputs.insert("skipped".into(), Value::Bool(true));
                        outputs.insert("headers".into(), Value::Map(BTreeMap::new()));
                        return Ok(outputs);
                    }
                    Overwrite::Never if exists => {
                        return Err(NodeError::Failed {
                            source_message: None,
                            message: format!(
                                "node '{node_id}': body_sink: file '{}' already exists and overwrite=never",
                                path.display()
                            ),
                            recoverable: false,
                        });
                    }
                    _ => {}
                }
            }

            // SSRF protection
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

            // Build reusable client with redirect policy.
            let client = reqwest::Client::builder()
                .redirect(redirect_cfg.clone().into_policy())
                .build()
                .map_err(|e| NodeError::Failed {
                    source_message: Some(e.to_string()),
                    message: format!("node '{node_id}': failed to build HTTP client: {e}"),
                    recoverable: false,
                })?;

            let parsed_url = reqwest::Url::parse(&url).map_err(|e| NodeError::Failed {
                source_message: Some(e.to_string()),
                message: format!("node '{node_id}': invalid URL '{url}': {e}"),
                recoverable: false,
            })?;

            // Materialise the non-multipart body bytes once — auth strategies
            // (HMAC) need to observe them, and retry attempts can reuse them.
            let body_bytes: Vec<u8> = if multipart_cfg.is_some() {
                Vec::new()
            } else if body_json {
                let json_body: serde_json::Value = inputs
                    .iter()
                    .map(|(k, v)| (k.clone(), value_to_json(v)))
                    .collect::<serde_json::Map<String, serde_json::Value>>()
                    .into();
                json_body.to_string().into_bytes()
            } else if let Some(tmpl) = &body_template {
                interpolate(tmpl, &inputs).into_bytes()
            } else {
                Vec::new()
            };

            // Resolve auth binding if needed (validates now; re-applied per attempt).
            let auth_binding: Option<AuthBinding> = if let Some(name) = &auth_name {
                let ctx = exec_ctx.as_ref().ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!(
                        "node '{node_id}': config.auth='{name}' requires an execution-context-bound HttpHandler"
                    ),
                    recoverable: false,
                })?;
                let registry = ctx.auth_registry().ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!(
                        "node '{node_id}': graph has no auth registry installed but config.auth='{name}' was set"
                    ),
                    recoverable: false,
                })?;
                let (_decl, strategy) =
                    registry
                        .get(name.as_str())
                        .ok_or_else(|| NodeError::Failed {
                            source_message: None,
                            message: format!(
                                "node '{node_id}': auth strategy '{name}' not declared in graph"
                            ),
                            recoverable: false,
                        })?;
                Some((name.clone(), strategy.clone(), ctx.clone()))
            } else {
                None
            };

            // Retry loop.
            let max_attempts = retry_cfg.as_ref().map(|r| r.max_attempts).unwrap_or(1);
            let mut attempt: u32 = 0;
            let response: Response = loop {
                attempt += 1;
                if cancel.is_cancelled() {
                    return Err(NodeError::Cancelled {
                        reason: "cancelled before HTTP attempt".into(),
                    });
                }

                // Build request builder for this attempt.
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
                request = request.timeout(Duration::from_millis(timeout_ms));

                // Headers.
                for (name, value_template) in &header_templates {
                    let value = interpolate(value_template, &inputs);
                    request = request.header(name.as_str(), value);
                }

                // Body.
                if let Some(mp) = multipart_cfg.as_ref() {
                    let form =
                        build_multipart_form(mp, &inputs)
                            .await
                            .map_err(|e| NodeError::Failed {
                                source_message: None,
                                message: format!("node '{node_id}': multipart build failed: {e}"),
                                recoverable: false,
                            })?;
                    request = request.multipart(form);
                } else if body_json {
                    request = request
                        .header("content-type", "application/json")
                        .body(body_bytes.clone());
                } else if !body_bytes.is_empty() {
                    request = request.body(body_bytes.clone());
                }

                // Auth apply — re-runs every attempt so bearer tokens can
                // rotate and HMAC re-signs.
                if let Some((name, strategy, ctx)) = auth_binding.as_ref() {
                    request = apply_auth(
                        &node_id,
                        ctx,
                        strategy.as_ref(),
                        name,
                        &inputs,
                        template.clone(),
                        &body_bytes,
                        &method,
                        &parsed_url,
                        request,
                    )
                    .await?;
                }

                // Execute with cancellation.
                let send_result = tokio::select! {
                    r = request.send() => r,
                    _ = cancel.cancelled() => {
                        return Err(NodeError::Cancelled {
                            reason: "cancelled during HTTP request".into(),
                        });
                    }
                };

                match send_result {
                    Ok(resp) => {
                        // Auth observe_response runs before we decide to retry
                        // so cookie-jar-style strategies see every response.
                        if let Some((name, strategy, ctx)) = auth_binding.as_ref() {
                            observe_auth(
                                &node_id,
                                ctx,
                                strategy.as_ref(),
                                name,
                                &inputs,
                                template.clone(),
                                &body_bytes,
                                &method,
                                &parsed_url,
                                resp.headers(),
                            )
                            .await?;
                        }

                        let status = resp.status().as_u16();
                        let should_retry = retry_cfg
                            .as_ref()
                            .map(|r| r.retry_on.matches_status(status))
                            .unwrap_or(false)
                            && attempt < max_attempts;

                        if should_retry {
                            let delay = retry_cfg.as_ref().unwrap().backoff.delay_for(attempt - 1);
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                _ = cancel.cancelled() => {
                                    return Err(NodeError::Cancelled {
                                        reason: "cancelled during HTTP retry backoff".into(),
                                    });
                                }
                            }
                            continue;
                        }
                        break resp;
                    }
                    Err(e) => {
                        // Connection / transport level error.
                        let is_conn_err = !e.is_timeout() && !e.is_status();
                        let should_retry = retry_cfg
                            .as_ref()
                            .map(|r| {
                                (r.retry_on.matches_conn_err() && is_conn_err)
                                    || (r.retry_on.matches_conn_err() && e.is_timeout())
                            })
                            .unwrap_or(false)
                            && attempt < max_attempts;
                        if should_retry {
                            let delay = retry_cfg.as_ref().unwrap().backoff.delay_for(attempt - 1);
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                _ = cancel.cancelled() => {
                                    return Err(NodeError::Cancelled {
                                        reason: "cancelled during HTTP retry backoff".into(),
                                    });
                                }
                            }
                            continue;
                        }
                        return Err(NodeError::Failed {
                            source_message: Some(e.to_string()),
                            message: format!("node '{node_id}': HTTP request failed: {e}"),
                            recoverable: e.is_timeout(),
                        });
                    }
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

            // If a body_sink is configured, stream (or skip) instead of buffering.
            if let Some((sink_path, _overwrite, create_parents)) = body_sink_resolved {
                // `fail_on_non_2xx`: fail BEFORE opening the file so we never
                // leave a partial / zero-byte file behind.
                if fail_on_non_2xx && !(200..300).contains(&status) {
                    let snippet = read_body_snippet(response, ERROR_BODY_SNIPPET_CAP).await;
                    return Err(NodeError::Failed {
                        source_message: Some(snippet.clone()),
                        message: format!(
                            "node '{node_id}': HTTP {status} (fail_on_non_2xx, body_sink not written); body={snippet}"
                        ),
                        recoverable: (500..600).contains(&status),
                    });
                }
                // Passthrough mode with a non-2xx response: writing an error
                // page to disk is rarely what the caller wants, so skip the
                // write and surface a capped snippet as a diagnostic. Status
                // + headers + (intended) path are still returned.
                if !(200..300).contains(&status) {
                    let snippet = read_body_snippet(response, ERROR_BODY_SNIPPET_CAP).await;
                    let mut outputs = Outputs::new();
                    outputs.insert("status".into(), Value::I64(status));
                    outputs.insert(
                        "path".into(),
                        Value::String(sink_path.to_string_lossy().into_owned()),
                    );
                    outputs.insert("bytes_written".into(), Value::I64(0));
                    outputs.insert("skipped".into(), Value::Bool(true));
                    outputs.insert("error_body_snippet".into(), Value::String(snippet));
                    outputs.insert("headers".into(), Value::Map(resp_headers));
                    return Ok(outputs);
                }

                let outcome = stream_response_to_file(
                    &node_id,
                    response,
                    &sink_path,
                    create_parents,
                    &cancel,
                )
                .await?;

                let mut outputs = Outputs::new();
                outputs.insert("status".into(), Value::I64(status));
                outputs.insert(
                    "path".into(),
                    Value::String(outcome.path.to_string_lossy().into_owned()),
                );
                outputs.insert(
                    "bytes_written".into(),
                    Value::I64(outcome.bytes_written as i64),
                );
                outputs.insert("headers".into(), Value::Map(resp_headers));
                return Ok(outputs);
            }

            let body = response.text().await.map_err(|e| NodeError::Failed {
                source_message: Some(e.to_string()),
                message: format!("node '{node_id}': failed to read response body: {e}"),
                recoverable: false,
            })?;

            if fail_on_non_2xx && !(200..300).contains(&status) {
                return Err(NodeError::Failed {
                    source_message: Some(body.clone()),
                    message: format!(
                        "node '{node_id}': HTTP {status} (fail_on_non_2xx); body={body}"
                    ),
                    recoverable: (500..600).contains(&status),
                });
            }

            // Schema validation (runs after fail_on_non_2xx; before output build).
            let validation_cfg_json = config.get("validation").cloned();
            let validation_result = if let Some(vjson) = validation_cfg_json.as_ref() {
                let validator = get_or_compile_validator(validators.as_ref(), vjson, &inputs)
                    .map_err(|e| NodeError::Failed {
                        source_message: None,
                        message: format!("node '{node_id}': {e}"),
                        recoverable: false,
                    })?;
                Some(run_validation(&validator, &body))
            } else {
                None
            };

            let mut outputs = Outputs::new();
            outputs.insert("status".into(), Value::I64(status));
            outputs.insert("body".into(), Value::String(body));
            outputs.insert("headers".into(), Value::Map(resp_headers));

            if let Some((mode, report)) = validation_result {
                apply_validation_report(&node_id, mode, report, &mut outputs)?;
            }

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
            .with_config(SchemaField::new("multipart", "object").describe(
                "{ fields: map<string,string>, files: [{name, path?|bytes?, filename?, content_type?}] }",
            ))
            .with_config(
                SchemaField::new("timeout_ms", "integer").default(serde_json::json!(30_000)),
            )
            .with_config(
                SchemaField::new("allow_private", "boolean")
                    .describe("Allow requests to private/loopback IPs")
                    .default(serde_json::json!(false)),
            )
            .with_config(
                SchemaField::new("auth", "string").describe("Name of graph-scoped auth strategy"),
            )
            .with_config(
                SchemaField::new("redirect", "string|object")
                    .describe("Redirect policy: \"none\", \"default\", or { limited: N }"),
            )
            .with_config(
                SchemaField::new("retry", "object").describe(
                    "{ max_attempts, backoff, delay_ms, multiplier, max_delay_ms, retry_on: [...] }",
                ),
            )
            .with_config(
                SchemaField::new("fail_on_non_2xx", "boolean")
                    .describe("Fail the node on non-2xx responses (default false: passthrough)")
                    .default(serde_json::json!(false)),
            )
            .with_config(
                SchemaField::new("validation", "object").describe(
                    "{ inline | file, on_failure: fail|passthrough } — JSON Schema validation \
                     of response body. Mutually exclusive with body_sink.",
                ),
            )
            .with_config(
                SchemaField::new("body_sink", "object").describe(
                    "{ file: { path, overwrite?: always|if_missing|never, create_parents?: bool } } — \
                     stream response body to disk instead of buffering",
                ),
            )
            .with_output(SchemaField::new("status", "integer"))
            .with_output(SchemaField::new("body", "string"))
            .with_output(SchemaField::new("headers", "map<string,string>"))
            .with_output(
                SchemaField::new("path", "string")
                    .describe("body_sink: final on-disk path"),
            )
            .with_output(
                SchemaField::new("bytes_written", "integer")
                    .describe("body_sink: number of bytes streamed to disk"),
            )
            .with_output(
                SchemaField::new("skipped", "boolean")
                    .describe("body_sink: true when write was skipped (IfMissing or non-2xx passthrough)"),
            )
            .with_output(
                SchemaField::new("validation_ok", "boolean")
                    .describe("Present only when config.validation is set"),
            )
            .with_output(
                SchemaField::new("validation_error", "object")
                    .describe("Failure details; only set in passthrough mode"),
            )
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_auth(
    node_id: &str,
    ctx: &Arc<ExecutionContext>,
    strategy: &dyn AuthStrategy,
    name: &str,
    inputs: &Outputs,
    template: Arc<dyn TemplateResolver>,
    body: &[u8],
    method: &str,
    url: &reqwest::Url,
    request: RequestBuilder,
) -> Result<RequestBuilder, NodeError> {
    let registry = ctx.auth_registry().ok_or_else(|| NodeError::Failed {
        source_message: None,
        message: format!("node '{node_id}': auth registry vanished mid-request"),
        recoverable: false,
    })?;
    let (decl, _) = registry.get(name).ok_or_else(|| NodeError::Failed {
        source_message: None,
        message: format!("node '{node_id}': auth strategy '{name}' disappeared mid-request"),
        recoverable: false,
    })?;
    let bb_snapshot = ctx.blackboard().clone();
    let apply_ctx = AuthApplyCtx {
        strategy_name: name,
        secrets_map: &decl.secrets,
        resolver: ctx.secret_resolver(),
        state: ctx.auth_state(),
        inputs,
        blackboard: &bb_snapshot,
        template,
        body,
        method,
        url,
    };
    strategy
        .apply(&apply_ctx, request)
        .await
        .map_err(|e| map_auth_error(node_id, &e))
}

#[allow(clippy::too_many_arguments)]
async fn observe_auth(
    node_id: &str,
    ctx: &Arc<ExecutionContext>,
    strategy: &dyn AuthStrategy,
    name: &str,
    inputs: &Outputs,
    template: Arc<dyn TemplateResolver>,
    body: &[u8],
    method: &str,
    url: &reqwest::Url,
    headers: &reqwest::header::HeaderMap,
) -> Result<(), NodeError> {
    let registry = ctx.auth_registry().ok_or_else(|| NodeError::Failed {
        source_message: None,
        message: format!("node '{node_id}': auth registry vanished mid-request"),
        recoverable: false,
    })?;
    let (decl, _) = registry.get(name).ok_or_else(|| NodeError::Failed {
        source_message: None,
        message: format!("node '{node_id}': auth strategy '{name}' disappeared mid-request"),
        recoverable: false,
    })?;
    let bb_snapshot = ctx.blackboard().clone();
    let apply_ctx = AuthApplyCtx {
        strategy_name: name,
        secrets_map: &decl.secrets,
        resolver: ctx.secret_resolver(),
        state: ctx.auth_state(),
        inputs,
        blackboard: &bb_snapshot,
        template,
        body,
        method,
        url,
    };
    strategy
        .observe_response(&apply_ctx, headers)
        .await
        .map_err(|e| map_auth_error(node_id, &e))
}

async fn build_multipart_form(cfg: &MultipartCfg, inputs: &Outputs) -> Result<Form, String> {
    let mut form = Form::new();
    for (name, value_tmpl) in &cfg.fields {
        let value = interpolate(value_tmpl, inputs);
        form = form.text(name.clone(), value);
    }
    for file in &cfg.files {
        match file {
            MultipartFile::Path {
                name,
                path,
                filename,
                content_type,
            } => {
                let resolved = interpolate(path, inputs);
                let bytes = tokio::fs::read(&resolved)
                    .await
                    .map_err(|e| format!("reading '{resolved}': {e}"))?;
                let fname = filename.clone().unwrap_or_else(|| {
                    std::path::Path::new(&resolved)
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("file")
                        .to_string()
                });
                let mut part = Part::bytes(bytes).file_name(fname);
                if let Some(ct) = content_type {
                    part = part
                        .mime_str(ct)
                        .map_err(|e| format!("invalid content_type '{ct}': {e}"))?;
                }
                form = form.part(name.clone(), part);
            }
            MultipartFile::Inline {
                name,
                bytes,
                filename,
                content_type,
            } => {
                let mut part = Part::bytes(bytes.clone());
                if let Some(fname) = filename {
                    part = part.file_name(fname.clone());
                }
                if let Some(ct) = content_type {
                    part = part
                        .mime_str(ct)
                        .map_err(|e| format!("invalid content_type '{ct}': {e}"))?;
                }
                form = form.part(name.clone(), part);
            }
        }
    }
    Ok(form)
}

fn map_auth_error(node_id: &str, err: &AuthError) -> NodeError {
    NodeError::Failed {
        source_message: Some(err.to_string()),
        message: format!("node '{node_id}': auth failed: {err}"),
        recoverable: err.is_recoverable(),
    }
}

/// Consume up to `cap` bytes of `response`'s body and return it as a UTF-8
/// string (lossy). Used for capped error snippets on `body_sink` non-2xx
/// short-circuits so callers get diagnostic signal without OOM risk on
/// multi-MB error pages.
async fn read_body_snippet(response: Response, cap: usize) -> String {
    let mut buf: Vec<u8> = Vec::with_capacity(cap.min(4096));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                let remaining = cap.saturating_sub(buf.len());
                if remaining == 0 {
                    break;
                }
                let take = remaining.min(bytes.len());
                buf.extend_from_slice(&bytes[..take]);
                if buf.len() >= cap {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// RAII guard that deletes a partial file on drop unless `disarm()` is
/// called. Used to clean up mid-stream write failures and cancellations.
struct PartialFileGuard {
    path: Option<PathBuf>,
}

impl PartialFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }
    fn disarm(mut self) {
        self.path = None;
    }
}

impl Drop for PartialFileGuard {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            // Best-effort removal; file may not exist if we never opened it.
            // Using std::fs here is fine — drop runs in caller context which
            // may or may not be async, and the call is a single unlink.
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Outcome of streaming a response body to disk.
struct BodySinkOutcome {
    path: PathBuf,
    bytes_written: u64,
}

/// Stream `response` body chunks to `path`. Honors `create_parents` (creates
/// all missing parent directories when true). Cooperatively cancels on
/// `cancel`. On any mid-stream error or cancellation the partial file is
/// removed before returning.
async fn stream_response_to_file(
    node_id: &str,
    response: Response,
    path: &StdPath,
    create_parents: bool,
    cancel: &CancellationToken,
) -> Result<BodySinkOutcome, NodeError> {
    if create_parents {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| NodeError::Failed {
                        source_message: Some(e.to_string()),
                        message: format!(
                            "node '{node_id}': body_sink: failed to create parent dir '{}': {e}",
                            parent.display()
                        ),
                        recoverable: false,
                    })?;
            }
        }
    }

    // Truncate-or-create. Retries land here with a fresh file so writes do
    // not append from a prior attempt.
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .await
        .map_err(|e| NodeError::Failed {
            source_message: Some(e.to_string()),
            message: format!(
                "node '{node_id}': body_sink: failed to open '{}': {e}",
                path.display()
            ),
            recoverable: false,
        })?;

    let guard = PartialFileGuard::new(path.to_path_buf());
    let mut writer = tokio::io::BufWriter::new(file);
    let mut stream = response.bytes_stream();
    let mut total: u64 = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                // Guard drop removes partial file.
                return Err(NodeError::Cancelled {
                    reason: "cancelled while streaming HTTP body to disk".into(),
                });
            }
            next = stream.next() => {
                match next {
                    Some(Ok(chunk)) => {
                        writer
                            .write_all(&chunk)
                            .await
                            .map_err(|e| NodeError::Failed {
                                source_message: Some(e.to_string()),
                                message: format!(
                                    "node '{node_id}': body_sink: write failed at {total} bytes: {e}"
                                ),
                                recoverable: false,
                            })?;
                        total += chunk.len() as u64;
                    }
                    Some(Err(e)) => {
                        return Err(NodeError::Failed {
                            source_message: Some(e.to_string()),
                            message: format!(
                                "node '{node_id}': body_sink: stream error at {total} bytes: {e}"
                            ),
                            recoverable: false,
                        });
                    }
                    None => break,
                }
            }
        }
    }

    writer.flush().await.map_err(|e| NodeError::Failed {
        source_message: Some(e.to_string()),
        message: format!("node '{node_id}': body_sink: flush failed: {e}"),
        recoverable: false,
    })?;
    writer.into_inner().sync_all().await.ok();

    guard.disarm();
    Ok(BodySinkOutcome {
        path: path.to_path_buf(),
        bytes_written: total,
    })
}

/// Check if a hostname resolves to a private/loopback/link-local address.
fn is_private_host(host: &str) -> bool {
    if host == "localhost" || host == "0.0.0.0" || host == "::1" || host == "[::1]" {
        return true;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified(),
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
        let result = HttpHandler::stateless()
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
        let result = HttpHandler::stateless()
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

        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), token)
            .await;
        assert!(matches!(result, Err(NodeError::Cancelled { .. })));
    }

    // -- SSRF protection --

    #[tokio::test]
    async fn blocks_localhost_by_default() {
        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({ "url": "http://localhost:8080/admin" });

        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private/loopback"));
    }

    #[tokio::test]
    async fn blocks_private_ip_by_default() {
        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({ "url": "http://192.168.1.1/secret" });

        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("private/loopback"));
    }

    #[tokio::test]
    async fn blocks_metadata_endpoint() {
        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({ "url": "http://169.254.169.254/latest/meta-data/" });

        let result = HttpHandler::stateless()
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

    // -- Config parsing --

    #[test]
    fn redirect_cfg_parses_variants() {
        assert!(matches!(
            RedirectCfg::from_config(None).unwrap(),
            RedirectCfg::Default
        ));
        assert!(matches!(
            RedirectCfg::from_config(Some(&serde_json::json!("none"))).unwrap(),
            RedirectCfg::None
        ));
        assert!(matches!(
            RedirectCfg::from_config(Some(&serde_json::json!("default"))).unwrap(),
            RedirectCfg::Default
        ));
        assert!(matches!(
            RedirectCfg::from_config(Some(&serde_json::json!({"limited": 3}))).unwrap(),
            RedirectCfg::Limited(3)
        ));
        assert!(RedirectCfg::from_config(Some(&serde_json::json!("weird"))).is_err());
    }

    #[test]
    fn retry_on_parses_tokens() {
        let r = RetryOn::parse(Some(&serde_json::json!(["5xx", "connection_error", 429]))).unwrap();
        assert!(r.any_5xx);
        assert!(r.connection_error);
        assert!(r.matches_status(500));
        assert!(r.matches_status(503));
        assert!(r.matches_status(429));
        assert!(!r.matches_status(404));
        assert!(r.matches_conn_err());
    }

    #[test]
    fn retry_on_default_has_conn_and_5xx() {
        let r = RetryOn::parse(None).unwrap();
        assert!(r.any_5xx);
        assert!(r.connection_error);
    }

    #[test]
    fn http_retry_config_parses() {
        let v = serde_json::json!({
            "max_attempts": 3,
            "backoff": "exponential",
            "delay_ms": 50,
            "multiplier": 2.0,
            "max_delay_ms": 500,
            "retry_on": ["5xx"]
        });
        let cfg = HttpRetryConfig::from_config(Some(&v)).unwrap().unwrap();
        assert_eq!(cfg.max_attempts, 3);
        assert!(matches!(cfg.backoff, HttpBackoff::Exponential { .. }));
        assert!(cfg.retry_on.any_5xx);
    }

    #[test]
    fn http_retry_config_returns_none_when_under_two_attempts() {
        let v = serde_json::json!({ "max_attempts": 1 });
        assert!(HttpRetryConfig::from_config(Some(&v)).unwrap().is_none());
    }

    #[test]
    fn multipart_cfg_parses_fields_and_files() {
        let v = serde_json::json!({
            "fields": { "foo": "bar-{x}" },
            "files": [
                { "name": "upload", "path": "/tmp/{fname}", "content_type": "text/plain" },
                { "name": "inline", "bytes": "hello bytes", "filename": "hi.txt" }
            ]
        });
        let cfg = MultipartCfg::from_config(&v).unwrap();
        assert_eq!(cfg.fields.len(), 1);
        assert_eq!(cfg.files.len(), 2);
    }

    #[tokio::test]
    async fn fail_on_non_2xx_flag_triggers_failure() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(503).set_body_string("broken"))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "allow_private": true,
            "fail_on_non_2xx": true,
        });
        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("503"));
        assert!(msg.contains("broken"));
    }

    #[tokio::test]
    async fn passthrough_preserves_status_by_default() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("nope"))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "allow_private": true,
        });
        let out = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("status").unwrap() {
            Value::I64(n) => assert_eq!(*n, 404),
            other => panic!("expected i64, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn redirect_none_returns_3xx() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/start"))
            .respond_with(wiremock::ResponseTemplate::new(302).append_header("location", "/end"))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/end"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("followed"))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": format!("{}/start", server.uri()),
            "allow_private": true,
            "redirect": "none",
        });
        let out = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("status").unwrap() {
            Value::I64(n) => assert_eq!(*n, 302),
            other => panic!("expected i64, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn redirect_default_follows() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/start"))
            .respond_with(wiremock::ResponseTemplate::new(302).append_header("location", "/end"))
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/end"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("followed"))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": format!("{}/start", server.uri()),
            "allow_private": true,
        });
        let out = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("status").unwrap() {
            Value::I64(n) => assert_eq!(*n, 200),
            other => panic!("expected i64, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retry_on_5xx_then_succeeds() {
        let server = wiremock::MockServer::start().await;
        // Up-front scenario: first hit 503, second hit 200. wiremock `Mock`
        // responses are sticky per route, so use `expect` to sequence.
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/flaky"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/flaky"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": format!("{}/flaky", server.uri()),
            "allow_private": true,
            "retry": {
                "max_attempts": 3,
                "backoff": "fixed",
                "delay_ms": 1,
                "retry_on": ["5xx"]
            }
        });
        let out = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("status").unwrap() {
            Value::I64(n) => assert_eq!(*n, 200),
            other => panic!("expected i64, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn retry_gives_up_after_max_attempts() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "allow_private": true,
            "retry": {
                "max_attempts": 2,
                "backoff": "fixed",
                "delay_ms": 1,
                "retry_on": ["5xx"]
            }
        });
        // With passthrough default, final 503 is returned as-is.
        let out = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("status").unwrap() {
            Value::I64(n) => assert_eq!(*n, 503),
            other => panic!("expected i64, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multipart_with_inline_bytes_roundtrip() {
        let server = wiremock::MockServer::start().await;
        // Match presence of multipart/form-data; we can't easily inspect
        // the body, but wiremock will accept any POST.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::header_regex(
                "content-type",
                "multipart/form-data; boundary=.+",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("got"))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "method": "POST",
            "allow_private": true,
            "multipart": {
                "fields": { "hello": "world" },
                "files": [
                    { "name": "blob", "bytes": "payload", "filename": "b.txt",
                      "content_type": "text/plain" }
                ]
            }
        });
        let out = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("status").unwrap() {
            Value::I64(n) => assert_eq!(*n, 200),
            other => panic!("expected i64, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn multipart_with_file_path_reads_disk() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::header_regex(
                "content-type",
                "multipart/form-data; boundary=.+",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"disk-contents").unwrap();

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "method": "POST",
            "allow_private": true,
            "multipart": {
                "fields": {},
                "files": [
                    { "name": "upload", "path": tmp.path().to_str().unwrap() }
                ]
            }
        });
        let out = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("status").unwrap() {
            Value::I64(n) => assert_eq!(*n, 200),
            other => panic!("expected i64, got {other:?}"),
        }
    }

    // -- Schema validation --

    #[tokio::test]
    async fn validation_fail_mode_passes_when_schema_matches() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_string(r#"{"id": 1, "name": "x"}"#),
            )
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "allow_private": true,
            "validation": {
                "inline": {
                    "type": "object",
                    "required": ["id", "name"],
                    "properties": {
                        "id": { "type": "integer" },
                        "name": { "type": "string" }
                    }
                }
            }
        });
        let out = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("validation_ok").unwrap() {
            Value::Bool(b) => assert!(*b),
            other => panic!("expected bool, got {other:?}"),
        }
        assert!(!out.contains_key("validation_error"));
    }

    #[tokio::test]
    async fn validation_fail_mode_fails_node_on_schema_miss() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_string(r#"{"id": "not-a-number"}"#),
            )
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "allow_private": true,
            "validation": {
                "inline": {
                    "type": "object",
                    "properties": { "id": { "type": "integer" } },
                    "required": ["id"]
                }
            }
        });
        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("schema validation failed"), "msg: {msg}");
    }

    #[tokio::test]
    async fn validation_passthrough_mode_attaches_error_field() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string(r#"{}"#))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
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
            .unwrap();
        match out.get("validation_ok").unwrap() {
            Value::Bool(b) => assert!(!*b),
            other => panic!("expected bool, got {other:?}"),
        }
        assert!(out.contains_key("validation_error"));
    }

    #[tokio::test]
    async fn validation_non_json_body_fails_in_fail_mode() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("plain text"))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "allow_private": true,
            "validation": { "inline": { "type": "object" } }
        });
        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not JSON"));
    }

    #[tokio::test]
    async fn validation_non_json_body_attaches_error_in_passthrough_mode() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("plain text"))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "allow_private": true,
            "validation": {
                "inline": { "type": "object" },
                "on_failure": "passthrough"
            }
        });
        let out = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("validation_ok").unwrap() {
            Value::Bool(b) => assert!(!*b),
            other => panic!("expected bool, got {other:?}"),
        }
        match out.get("validation_error").unwrap() {
            Value::Map(m) => {
                assert_eq!(m.get("kind"), Some(&Value::String("not_json".into())));
            }
            other => panic!("expected map, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn validation_skipped_when_fail_on_non_2xx_already_failed() {
        // Validation must not observe a body that was rejected upstream.
        // If the node already fails on non-2xx, validation never runs.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("not json"))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "allow_private": true,
            "fail_on_non_2xx": true,
            "validation": { "inline": { "type": "object" } }
        });
        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        // Error message reflects fail_on_non_2xx, not the schema path.
        let err = result.unwrap_err().to_string();
        assert!(err.contains("500"));
        assert!(!err.contains("schema validation"));
    }

    #[tokio::test]
    async fn validation_bad_config_surfaces_construction_error() {
        // Setting both `inline` and `file` is rejected at config parse time.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": server.uri(),
            "allow_private": true,
            "validation": {
                "inline": { "type": "object" },
                "file": "/tmp/nope.json"
            }
        });
        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exactly one"));
    }

    // -- body_sink config parsing --

    #[test]
    fn body_sink_absent_returns_none() {
        assert!(BodySinkCfg::from_config(None).unwrap().is_none());
    }

    #[test]
    fn body_sink_file_parses_with_defaults() {
        let v = serde_json::json!({ "file": { "path": "/tmp/out.bin" } });
        let cfg = BodySinkCfg::from_config(Some(&v)).unwrap().unwrap();
        match cfg {
            BodySinkCfg::File {
                path_template,
                overwrite,
                create_parents,
            } => {
                assert_eq!(path_template, "/tmp/out.bin");
                assert_eq!(overwrite, Overwrite::Always);
                assert!(create_parents);
            }
        }
    }

    #[test]
    fn body_sink_file_parses_overwrite_and_create_parents() {
        let v = serde_json::json!({
            "file": {
                "path": "/tmp/{name}.bin",
                "overwrite": "if_missing",
                "create_parents": false,
            }
        });
        let cfg = BodySinkCfg::from_config(Some(&v)).unwrap().unwrap();
        match cfg {
            BodySinkCfg::File {
                overwrite,
                create_parents,
                ..
            } => {
                assert_eq!(overwrite, Overwrite::IfMissing);
                assert!(!create_parents);
            }
        }
    }

    #[test]
    fn body_sink_rejects_missing_path() {
        let v = serde_json::json!({ "file": {} });
        assert!(BodySinkCfg::from_config(Some(&v)).is_err());
    }

    #[test]
    fn body_sink_rejects_missing_file_key() {
        let v = serde_json::json!({ "path": "/tmp/out.bin" });
        assert!(BodySinkCfg::from_config(Some(&v)).is_err());
    }

    #[test]
    fn overwrite_parses_strings_and_bools() {
        assert_eq!(Overwrite::parse(None).unwrap(), Overwrite::Always);
        assert_eq!(
            Overwrite::parse(Some(&serde_json::json!("always"))).unwrap(),
            Overwrite::Always,
        );
        assert_eq!(
            Overwrite::parse(Some(&serde_json::json!("if_missing"))).unwrap(),
            Overwrite::IfMissing,
        );
        assert_eq!(
            Overwrite::parse(Some(&serde_json::json!("never"))).unwrap(),
            Overwrite::Never,
        );
        assert_eq!(
            Overwrite::parse(Some(&serde_json::json!(true))).unwrap(),
            Overwrite::Always,
        );
        assert_eq!(
            Overwrite::parse(Some(&serde_json::json!(false))).unwrap(),
            Overwrite::Never,
        );
        assert!(Overwrite::parse(Some(&serde_json::json!("maybe"))).is_err());
    }

    #[tokio::test]
    async fn body_sink_and_validation_conflict_rejected_at_config_load() {
        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": "http://example.com",
            "allow_private": true,
            "body_sink": { "file": { "path": "/tmp/out.bin" } },
            "validation": { "inline": { "type": "object" } }
        });
        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("mutually exclusive"), "msg: {msg}");
    }

    #[tokio::test]
    async fn body_sink_never_with_existing_file_errors_before_request() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"existing").unwrap();

        let mut node = Node::new("H", "Http");
        // URL is never reached — short-circuit happens before the hop.
        node.config = serde_json::json!({
            "url": "http://example.com",
            "allow_private": true,
            "body_sink": {
                "file": {
                    "path": tmp.path().to_str().unwrap(),
                    "overwrite": "never",
                }
            }
        });
        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
        // Existing contents untouched.
        let contents = std::fs::read(tmp.path()).unwrap();
        assert_eq!(contents, b"existing");
    }

    #[tokio::test]
    async fn body_sink_if_missing_with_existing_file_skips_request() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), b"pre-existing").unwrap();

        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": "http://example.com",
            "allow_private": true,
            "body_sink": {
                "file": {
                    "path": tmp.path().to_str().unwrap(),
                    "overwrite": "if_missing",
                }
            }
        });
        let out = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .unwrap();
        match out.get("skipped").unwrap() {
            Value::Bool(b) => assert!(*b),
            other => panic!("expected bool, got {other:?}"),
        }
        // File untouched.
        let contents = std::fs::read(tmp.path()).unwrap();
        assert_eq!(contents, b"pre-existing");
    }

    #[tokio::test]
    async fn multipart_missing_path_fails_cleanly() {
        let mut node = Node::new("H", "Http");
        node.config = serde_json::json!({
            "url": "http://example.com",
            "method": "POST",
            "allow_private": true,
            "multipart": {
                "files": [
                    { "name": "upload", "path": "/definitely/does/not/exist.xyz" }
                ]
            }
        });
        let result = HttpHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("multipart"));
    }
}
