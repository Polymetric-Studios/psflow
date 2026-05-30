//! WebSocket node handler.
//!
//! Opens a WS connection to a templated URL, optionally sends init frames,
//! then reads received frames until one of the configured termination
//! triggers fires (predicate, max frame count, wall-clock timeout, external
//! cancellation, server close, validation failure).
//!
//! Received frames can either be collected into a `Vec<Value>` (`emit: collect`)
//! or streamed to disk one JSON-per-line (`emit: sink_file`). Per-frame JSON
//! Schema validation reuses the transport-agnostic
//! [`crate::validation::CompiledValidator`] from the HTTP handler — the
//! same pattern, applied to each received frame instead of a single response
//! body.
//!
//! Auth runs via the same graph-scoped [`crate::auth::AuthStrategy`] layer
//! as the HTTP handler. The WS-handshake variant of the trait
//! ([`AuthStrategy::apply_ws_request`]) decorates the `http::Request<()>`
//! that `tokio-tungstenite` uses for the upgrade.

use crate::auth::{AuthApplyCtx, AuthError, AuthStrategy};
use crate::error::NodeError;
use crate::execute::validation::{ValidationIssue, ValidationIssueKind};
use crate::execute::{
    CancellationToken, ExecutionContext, HandlerSchema, NodeHandler, Outputs, SchemaField,
};
use crate::graph::node::Node;
use crate::graph::types::Value;
use crate::graph::Graph;
use crate::handlers::common::interpolate;
use crate::scripting::bridge::value_to_dynamic;
use crate::scripting::engine::ScriptEngine;
use crate::template::{PromptTemplateResolver, TemplateResolver};
use crate::validation::{CompiledValidator, FailureMode, ValidationConfig, ValidationOutcome};
use futures::{SinkExt, StreamExt};
use rhai::{Dynamic, Scope, AST};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::Message;

/// The handler name this module registers under. Mirrors `HttpHandler`.
pub const WS_HANDLER_NAME: &str = "ws";

/// WebSocket node handler.
///
/// See module docs for behavior.
pub struct WebSocketHandler {
    exec_ctx: Option<Arc<ExecutionContext>>,
    template: Arc<dyn TemplateResolver>,
    /// Compiled per-frame validators, keyed by the raw JSON text of
    /// `config.validation`.
    validators: Arc<Mutex<HashMap<String, CompiledValidator>>>,
    /// Rhai engine used to compile and evaluate termination predicates.
    /// Shared across handler invocations.
    script_engine: Arc<ScriptEngine>,
    /// Compiled predicate ASTs, keyed by their source string.
    predicate_asts: Arc<Mutex<HashMap<String, Arc<AST>>>>,
}

impl WebSocketHandler {
    /// Handler without auth context. Requests that reference `config.auth`
    /// will fail.
    pub fn stateless() -> Self {
        Self {
            exec_ctx: None,
            template: Arc::new(PromptTemplateResolver),
            validators: Arc::new(Mutex::new(HashMap::new())),
            script_engine: Arc::new(ScriptEngine::with_defaults()),
            predicate_asts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Handler bound to an execution context. Required for `config.auth` to
    /// resolve against the graph's auth registry.
    pub fn new(ctx: Arc<ExecutionContext>) -> Self {
        Self {
            exec_ctx: Some(ctx),
            template: Arc::new(PromptTemplateResolver),
            validators: Arc::new(Mutex::new(HashMap::new())),
            script_engine: Arc::new(ScriptEngine::with_defaults()),
            predicate_asts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_template_resolver(mut self, resolver: Arc<dyn TemplateResolver>) -> Self {
        self.template = resolver;
        self
    }

    pub fn with_script_engine(mut self, engine: Arc<ScriptEngine>) -> Self {
        self.script_engine = engine;
        self
    }
}

impl Default for WebSocketHandler {
    fn default() -> Self {
        Self::stateless()
    }
}

// -- config types --------------------------------------------------------------

/// An outgoing init frame. Template-interpolated from node inputs before send.
#[derive(Debug, Clone)]
enum WsFrame {
    Text(String),
    Binary(Vec<u8>),
}

/// How received frames surface in the output.
#[derive(Debug, Clone)]
#[allow(dead_code)]
enum StreamEmitMode {
    /// Accumulate every received (post-validation) frame into a `Vec<Value>`.
    Collect,
    /// Stream JSON-encoded frames one per line to a file on disk.
    SinkFile {
        path_template: String,
        create_parents: bool,
        overwrite: SinkOverwrite,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkOverwrite {
    Always,
    IfMissing,
    Never,
}

impl SinkOverwrite {
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
                    "sink_file.overwrite: expected \"always\" | \"if_missing\" | \"never\", got '{other}'"
                )),
            };
        }
        if let Some(b) = v.as_bool() {
            return Ok(if b { Self::Always } else { Self::Never });
        }
        Err("sink_file.overwrite must be a string or a bool".into())
    }
}

/// Parsed `config.terminate` block.
#[derive(Debug, Clone, Default)]
struct TerminateCfg {
    /// Optional Rhai predicate source — receives the received frame as
    /// variable `frame` plus `frame_index`. Must return bool.
    predicate: Option<String>,
    max_frames: Option<u32>,
    timeout_ms: Option<u64>,
    /// Send a close frame before returning. Default true.
    close_on_terminate: bool,
}

impl TerminateCfg {
    fn from_config(v: Option<&serde_json::Value>) -> Result<Self, String> {
        let Some(v) = v else {
            return Ok(Self {
                close_on_terminate: true,
                ..Default::default()
            });
        };
        let obj = v
            .as_object()
            .ok_or_else(|| "terminate must be an object".to_string())?;
        let predicate = obj
            .get("on_predicate")
            .and_then(|v| v.as_str())
            .map(String::from);
        let max_frames = obj
            .get("max_frames")
            .and_then(|v| v.as_u64())
            .map(|n| n.min(u32::MAX as u64) as u32);
        let timeout_ms = obj.get("timeout_ms").and_then(|v| v.as_u64());
        let close_on_terminate = obj
            .get("close_on_terminate")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        Ok(Self {
            predicate,
            max_frames,
            timeout_ms,
            close_on_terminate,
        })
    }
}

/// Default HTTP method for a handshake `auth_request`.
const HANDSHAKE_DEFAULT_METHOD: &str = "POST";
/// Default `Content-Type` for a handshake `auth_request`.
const HANDSHAKE_DEFAULT_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

/// Parsed `config.handshake.auth_request` block — the mid-stream HTTP POST
/// made to an auth endpoint during a reactive subscribe handshake.
#[derive(Debug, Clone)]
struct AuthRequestCfg {
    /// URL template; `{inputs.x}` interpolation via [`interpolate`].
    url: String,
    /// HTTP method. Defaults to `POST`.
    method: String,
    /// `Content-Type` header. Defaults to `application/x-www-form-urlencoded`.
    content_type: String,
    /// Rhai script (scope: `frame`, `inputs`) returning the request body.
    body: String,
    /// Optional graph auth strategy name applied via `AuthStrategy::apply`.
    auth: Option<String>,
}

impl AuthRequestCfg {
    fn from_json(v: &serde_json::Value) -> Result<Self, String> {
        let obj = v
            .as_object()
            .ok_or_else(|| "handshake.auth_request must be an object".to_string())?;
        let url = obj
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "handshake.auth_request.url is required".to_string())?
            .to_string();
        let method = obj
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or(HANDSHAKE_DEFAULT_METHOD)
            .to_string();
        let content_type = obj
            .get("content_type")
            .and_then(|v| v.as_str())
            .unwrap_or(HANDSHAKE_DEFAULT_CONTENT_TYPE)
            .to_string();
        let body = obj
            .get("body")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "handshake.auth_request.body is required".to_string())?
            .to_string();
        let auth = obj.get("auth").and_then(|v| v.as_str()).map(String::from);
        Ok(Self {
            url,
            method,
            content_type,
            body,
            auth,
        })
    }
}

/// Parsed `config.handshake` block — a reactive subscribe handshake.
///
/// On a triggering data frame the handler optionally makes a mid-stream HTTP
/// request (`auth_request`) and then sends a computed `send` frame. The
/// triggering frame is consumed by the handshake and excluded from normal
/// frame handling.
#[derive(Debug, Clone)]
struct HandshakeCfg {
    /// Rhai predicate (scope: `frame`) deciding whether a frame triggers the
    /// handshake. Compiled as an expression.
    trigger: String,
    /// Optional mid-stream auth request. Omitted for public channels.
    auth_request: Option<AuthRequestCfg>,
    /// Rhai script (scope: `frame`, `inputs`, `auth`) returning the frame text
    /// to send back. Compiled as a full script.
    send: String,
}

impl HandshakeCfg {
    fn from_json(v: Option<&serde_json::Value>) -> Result<Option<Self>, String> {
        let Some(v) = v else {
            return Ok(None);
        };
        let obj = v
            .as_object()
            .ok_or_else(|| "handshake must be an object".to_string())?;
        let trigger = obj
            .get("trigger")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "handshake.trigger is required".to_string())?
            .to_string();
        let auth_request = match obj.get("auth_request") {
            Some(ar) => Some(AuthRequestCfg::from_json(ar)?),
            None => None,
        };
        let send = obj
            .get("send")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "handshake.send is required".to_string())?
            .to_string();
        Ok(Some(Self {
            trigger,
            auth_request,
            send,
        }))
    }
}

/// Fully parsed WebSocket config — the surface the node's `config` JSON
/// lowers to.
struct WebSocketConfig {
    url_template: String,
    auth_name: Option<String>,
    init_frames: Vec<WsFrame>,
    validation_cfg: Option<serde_json::Value>,
    terminate: TerminateCfg,
    emit: StreamEmitMode,
    subprotocol: Option<String>,
    handshake: Option<HandshakeCfg>,
}

impl WebSocketConfig {
    fn from_json(config: &serde_json::Value) -> Result<Self, String> {
        let url_template = config
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing config.url".to_string())?
            .to_string();
        let auth_name = config
            .get("auth")
            .and_then(|v| v.as_str())
            .map(String::from);
        let init_frames = parse_init_frames(config.get("init_frames"))?;
        let validation_cfg = config.get("validation").cloned();
        let terminate = TerminateCfg::from_config(config.get("terminate"))?;
        let emit = parse_emit(config.get("emit"))?;
        let subprotocol = config
            .get("subprotocol")
            .and_then(|v| v.as_str())
            .map(String::from);
        let handshake = HandshakeCfg::from_json(config.get("handshake"))?;
        Ok(Self {
            url_template,
            auth_name,
            init_frames,
            validation_cfg,
            terminate,
            emit,
            subprotocol,
            handshake,
        })
    }
}

fn parse_init_frames(v: Option<&serde_json::Value>) -> Result<Vec<WsFrame>, String> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| "init_frames must be an array".to_string())?;
    let mut frames = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        // Accept two shapes:
        // - "a raw string"  → Text frame
        // - { "text": "..." } | { "binary": "..." } | { "binary_bytes": [u8, ...] }
        if let Some(s) = entry.as_str() {
            frames.push(WsFrame::Text(s.to_string()));
            continue;
        }
        let obj = entry.as_object().ok_or_else(|| {
            format!("init_frames[{i}] must be a string or object with 'text'/'binary'")
        })?;
        if let Some(t) = obj.get("text").and_then(|v| v.as_str()) {
            frames.push(WsFrame::Text(t.to_string()));
        } else if let Some(b) = obj.get("binary").and_then(|v| v.as_str()) {
            frames.push(WsFrame::Binary(b.as_bytes().to_vec()));
        } else if let Some(arr) = obj.get("binary_bytes").and_then(|v| v.as_array()) {
            let mut buf = Vec::with_capacity(arr.len());
            for b in arr {
                let n = b
                    .as_u64()
                    .ok_or_else(|| format!("init_frames[{i}].binary_bytes must be u8 values"))?;
                if n > 255 {
                    return Err(format!(
                        "init_frames[{i}].binary_bytes: value {n} out of u8 range"
                    ));
                }
                buf.push(n as u8);
            }
            frames.push(WsFrame::Binary(buf));
        } else {
            return Err(format!(
                "init_frames[{i}] requires one of 'text', 'binary', or 'binary_bytes'"
            ));
        }
    }
    Ok(frames)
}

fn parse_emit(v: Option<&serde_json::Value>) -> Result<StreamEmitMode, String> {
    // Accept three shapes:
    // - absent              → Collect
    // - "collect"           → Collect
    // - { "collect": ... }  → Collect
    // - { "sink_file": { "path": "...", "overwrite"?: "...", "create_parents"?: bool } }
    let Some(v) = v else {
        return Ok(StreamEmitMode::Collect);
    };
    if let Some(s) = v.as_str() {
        return match s {
            "collect" => Ok(StreamEmitMode::Collect),
            other => Err(format!(
                "emit: unknown string variant '{other}' (expected \"collect\")"
            )),
        };
    }
    let obj = v
        .as_object()
        .ok_or_else(|| "emit must be a string or object".to_string())?;
    if obj.contains_key("collect") {
        return Ok(StreamEmitMode::Collect);
    }
    if let Some(sf) = obj.get("sink_file") {
        let sf_obj = sf
            .as_object()
            .ok_or_else(|| "emit.sink_file must be an object".to_string())?;
        let path_template = sf_obj
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "emit.sink_file.path is required".to_string())?
            .to_string();
        let create_parents = sf_obj
            .get("create_parents")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let overwrite = SinkOverwrite::parse(sf_obj.get("overwrite"))?;
        return Ok(StreamEmitMode::SinkFile {
            path_template,
            create_parents,
            overwrite,
        });
    }
    Err("emit: expected \"collect\" or { sink_file: { path, ... } }".into())
}

// -- validation glue ----------------------------------------------------------

/// Per-frame validation report. Same three-way shape as the HTTP handler's
/// [`crate::validation`] wrapper — the frame may be valid, invalid, or not
/// JSON at all (binary frames or text that fails to parse).
enum FrameValidationReport {
    Valid,
    Invalid {
        errors: Vec<crate::validation::ValidationFailure>,
    },
    NotJson {
        reason: String,
    },
}

fn run_frame_validation(
    validator: &CompiledValidator,
    parsed: Option<&serde_json::Value>,
    parse_error: Option<String>,
) -> FrameValidationReport {
    match (parsed, parse_error) {
        (Some(json), _) => match validator.validate(json) {
            ValidationOutcome::Valid => FrameValidationReport::Valid,
            ValidationOutcome::Invalid { errors } => FrameValidationReport::Invalid { errors },
        },
        (None, Some(reason)) => FrameValidationReport::NotJson { reason },
        (None, None) => FrameValidationReport::NotJson {
            reason: "frame is binary; validation requires JSON".to_string(),
        },
    }
}

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
    let resolver = |tmpl: &str| PathBuf::from(interpolate(tmpl, inputs));
    let validator = CompiledValidator::from_config(&cfg, resolver).map_err(|e| e.to_string())?;
    cache.lock().unwrap().insert(key, validator.clone());
    Ok(validator)
}

// -- output building ----------------------------------------------------------

/// Reason the session ended. Serialised as a lowercase string into the
/// `terminated_by` output field.
///
/// `ValidationError` is carried through the API so downstream graphs can
/// branch on it even though the current handler fails the node (rather than
/// completing with a reason) when validation is in `fail` mode. It is
/// reachable from embedder-layered wrappers that catch the validation
/// failure and surface a clean completion.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum TerminationReason {
    Predicate,
    MaxFrames,
    Timeout,
    Cancelled,
    ServerClose,
    ValidationError,
}

impl TerminationReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Predicate => "predicate",
            Self::MaxFrames => "max_frames",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::ServerClose => "server_close",
            Self::ValidationError => "validation_error",
        }
    }
}

/// A single surfaced frame for the `collect` emit mode.
///
/// `kind` is `"text"` or `"binary"`. `json` is populated when the text frame
/// parsed as JSON. `text` is the raw UTF-8. `binary_bytes` lists the raw
/// bytes for binary frames. A `validation_error` field appears on passthrough
/// failures.
fn frame_to_value(
    kind: &str,
    raw_text: Option<&str>,
    raw_bytes: Option<&[u8]>,
    parsed_json: Option<&serde_json::Value>,
    validation_error: Option<&serde_json::Value>,
) -> Value {
    let mut map: BTreeMap<String, Value> = BTreeMap::new();
    map.insert("kind".into(), Value::String(kind.into()));
    if let Some(t) = raw_text {
        map.insert("text".into(), Value::String(t.into()));
    }
    if let Some(b) = raw_bytes {
        map.insert(
            "binary_bytes".into(),
            Value::Vec(b.iter().map(|x| Value::I64(*x as i64)).collect()),
        );
    }
    if let Some(j) = parsed_json {
        map.insert("json".into(), json_to_value(j));
    }
    if let Some(err) = validation_error {
        map.insert("validation_error".into(), json_to_value(err));
    }
    Value::Map(map)
}

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

// -- predicate ---------------------------------------------------------------

fn compile_predicate(
    engine: &ScriptEngine,
    cache: &Mutex<HashMap<String, Arc<AST>>>,
    source: &str,
) -> Result<Arc<AST>, String> {
    if let Some(ast) = cache.lock().unwrap().get(source).cloned() {
        return Ok(ast);
    }
    let ast = engine
        .compile_expression(source)
        .map_err(|e| format!("terminate.on_predicate: {e}"))?;
    let arc = Arc::new(ast);
    cache
        .lock()
        .unwrap()
        .insert(source.to_string(), arc.clone());
    Ok(arc)
}

/// Evaluate a compiled predicate with `frame` + `frame_index` variables in
/// scope. Returns true iff the script returned a truthy bool.
fn eval_predicate(
    engine: &ScriptEngine,
    ast: &AST,
    frame_value: &Value,
    frame_index: u32,
    cancel: &CancellationToken,
) -> Result<bool, String> {
    let mut scope = Scope::new();
    scope.push_dynamic("frame", value_to_dynamic(frame_value));
    scope.push_dynamic("frame_index", Dynamic::from(frame_index as i64));
    match engine.eval_ast(&mut scope, ast, cancel) {
        Ok(d) => Ok(d.as_bool().unwrap_or(false)),
        Err(e) => Err(format!("terminate.on_predicate: {e}")),
    }
}

// -- handshake scripting -----------------------------------------------------

/// Compile a full Rhai script (may contain statements; returns a value) into
/// a cached AST. Used for the handshake `body` and `send` scripts, which —
/// unlike the terminate predicate — need statement support (e.g. parsing the
/// connection_established `data` JSON string via `let d = parse_json(...)`).
fn compile_script(
    engine: &ScriptEngine,
    cache: &Mutex<HashMap<String, Arc<AST>>>,
    label: &str,
    source: &str,
) -> Result<Arc<AST>, String> {
    if let Some(ast) = cache.lock().unwrap().get(source).cloned() {
        return Ok(ast);
    }
    let ast = engine
        .compile(source)
        .map_err(|e| format!("{label}: {e}"))?;
    let arc = Arc::new(ast);
    cache
        .lock()
        .unwrap()
        .insert(source.to_string(), arc.clone());
    Ok(arc)
}

/// Evaluate the handshake trigger predicate against a single frame. Scope:
/// `frame` only. Returns true iff the script returned a truthy bool.
fn eval_handshake_trigger(
    engine: &ScriptEngine,
    ast: &AST,
    frame_value: &Value,
    cancel: &CancellationToken,
) -> Result<bool, String> {
    let mut scope = Scope::new();
    scope.push_dynamic("frame", value_to_dynamic(frame_value));
    match engine.eval_ast(&mut scope, ast, cancel) {
        Ok(d) => Ok(d.as_bool().unwrap_or(false)),
        Err(e) => Err(format!("handshake.trigger: {e}")),
    }
}

/// Evaluate a handshake string-returning script. Scope: `frame`, `inputs`, and
/// (when `auth` is `Some`) `auth`. Returns the produced String.
fn eval_handshake_string(
    engine: &ScriptEngine,
    ast: &AST,
    label: &str,
    frame_value: &Value,
    inputs: &Outputs,
    auth: Option<&serde_json::Value>,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let mut scope = Scope::new();
    scope.push_dynamic("frame", value_to_dynamic(frame_value));
    scope.push_dynamic(
        "inputs",
        Dynamic::from_map(crate::scripting::bridge::outputs_to_rhai_map(inputs)),
    );
    if let Some(auth) = auth {
        scope.push_dynamic("auth", json_to_dynamic(auth));
    }
    match engine.eval_ast(&mut scope, ast, cancel) {
        Ok(d) => d
            .into_string()
            .map_err(|actual| format!("{label}: expected a String, got {actual}")),
        Err(e) => Err(format!("{label}: {e}")),
    }
}

/// Convert a `serde_json::Value` into a Rhai `Dynamic` for script scope.
fn json_to_dynamic(v: &serde_json::Value) -> Dynamic {
    value_to_dynamic(&json_to_value(v))
}

// -- auth glue ---------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn apply_ws_auth(
    node_id: &str,
    ctx: &Arc<ExecutionContext>,
    strategy: &dyn AuthStrategy,
    name: &str,
    inputs: &Outputs,
    template: Arc<dyn TemplateResolver>,
    url: &reqwest::Url,
    request: http::Request<()>,
) -> Result<http::Request<()>, NodeError> {
    let registry = ctx.auth_registry().ok_or_else(|| NodeError::Failed {
        source_message: None,
        message: format!("node '{node_id}': auth registry vanished mid-handshake"),
        recoverable: false,
    })?;
    let (decl, _) = registry.get(name).ok_or_else(|| NodeError::Failed {
        source_message: None,
        message: format!("node '{node_id}': auth strategy '{name}' disappeared mid-handshake"),
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
        // WS handshakes have no body and always use GET.
        body: &[],
        method: "GET",
        url,
    };
    strategy
        .apply_ws_request(&apply_ctx, request)
        .await
        .map_err(|e| map_auth_error(node_id, &e))
}

fn map_auth_error(node_id: &str, err: &AuthError) -> NodeError {
    NodeError::Failed {
        source_message: Some(err.to_string()),
        message: format!("node '{node_id}': auth failed: {err}"),
        recoverable: err.is_recoverable(),
    }
}

/// Apply a graph auth strategy to the handshake's HTTP `auth_request` POST.
///
/// Mirrors the HTTP handler's `apply_auth`: builds an [`AuthApplyCtx`] with the
/// real method, body bytes, and parsed URL, then calls
/// [`AuthStrategy::apply`] on a `reqwest::RequestBuilder`.
#[allow(clippy::too_many_arguments)]
async fn apply_handshake_auth(
    node_id: &str,
    ctx: &Arc<ExecutionContext>,
    name: &str,
    inputs: &Outputs,
    template: Arc<dyn TemplateResolver>,
    body: &[u8],
    method: &str,
    url: &reqwest::Url,
    builder: reqwest::RequestBuilder,
) -> Result<reqwest::RequestBuilder, NodeError> {
    let registry = ctx.auth_registry().ok_or_else(|| NodeError::Failed {
        source_message: None,
        message: format!("node '{node_id}': auth registry vanished mid-handshake"),
        recoverable: false,
    })?;
    let (decl, strategy) = registry.get(name).ok_or_else(|| NodeError::Failed {
        source_message: None,
        message: format!(
            "node '{node_id}': handshake auth strategy '{name}' not declared in graph"
        ),
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
        .apply(&apply_ctx, builder)
        .await
        .map_err(|e| map_auth_error(node_id, &e))
}

// -- compiled handshake -------------------------------------------------------

/// Compiled, ready-to-run handshake. Built once in `execute()` from
/// [`HandshakeCfg`]; the trigger predicate, the optional `auth_request.body`
/// script, and the `send` script are pre-compiled like the terminate predicate.
struct CompiledHandshake {
    trigger: Arc<AST>,
    auth_request: Option<CompiledAuthRequest>,
    send: Arc<AST>,
}

struct CompiledAuthRequest {
    url_template: String,
    method: String,
    content_type: String,
    body: Arc<AST>,
    auth: Option<String>,
}

/// Run the reactive handshake against a triggering frame: optionally POST to
/// the auth endpoint, then evaluate and send the computed subscribe frame.
///
/// Returns the `Message::Text` to send back over the WebSocket.
#[allow(clippy::too_many_arguments)]
async fn run_handshake(
    node_id: &str,
    handshake: &CompiledHandshake,
    frame_value: &Value,
    inputs: &Outputs,
    exec_ctx: &Option<Arc<ExecutionContext>>,
    template: &Arc<dyn TemplateResolver>,
    script_engine: &ScriptEngine,
    cancel: &CancellationToken,
) -> Result<Message, NodeError> {
    let fail = |message: String| NodeError::Failed {
        source_message: None,
        message: format!("node '{node_id}': {message}"),
        recoverable: false,
    };

    // -- optional auth_request POST -----------------------------------------
    let auth_json = if let Some(ar) = handshake.auth_request.as_ref() {
        let url_str = interpolate(&ar.url_template, inputs);
        let parsed_url = reqwest::Url::parse(&url_str).map_err(|e| {
            fail(format!(
                "handshake.auth_request: invalid URL '{url_str}': {e}"
            ))
        })?;
        let body = eval_handshake_string(
            script_engine,
            &ar.body,
            "handshake.auth_request.body",
            frame_value,
            inputs,
            None,
            cancel,
        )
        .map_err(fail)?;

        let client = reqwest::Client::new();
        let mut builder = client
            .request(
                reqwest::Method::from_bytes(ar.method.as_bytes())
                    .map_err(|e| fail(format!("handshake.auth_request: invalid method: {e}")))?,
                parsed_url.clone(),
            )
            .header(reqwest::header::CONTENT_TYPE, ar.content_type.clone())
            .body(body.clone().into_bytes());

        if let Some(name) = ar.auth.as_ref() {
            let ctx = exec_ctx.as_ref().ok_or_else(|| {
                fail(format!(
                    "handshake.auth_request.auth='{name}' requires an execution-context-bound WebSocketHandler"
                ))
            })?;
            builder = apply_handshake_auth(
                node_id,
                ctx,
                name,
                inputs,
                template.clone(),
                body.as_bytes(),
                &ar.method,
                &parsed_url,
                builder,
            )
            .await?;
        }

        let resp = tokio::select! {
            r = builder.send() => r.map_err(|e| {
                fail(format!("handshake.auth_request: send failed: {e}"))
            })?,
            _ = cancel.cancelled() => {
                return Err(NodeError::Cancelled {
                    reason: "cancelled during handshake auth_request".into(),
                });
            }
        };
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| fail(format!("handshake.auth_request: read body failed: {e}")))?;
        if !status.is_success() {
            return Err(fail(format!(
                "handshake.auth_request: non-2xx status {status}: {text}"
            )));
        }
        serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| fail(format!("handshake.auth_request: response not JSON: {e}")))?
    } else {
        serde_json::Value::Object(serde_json::Map::new())
    };

    // -- computed send frame -------------------------------------------------
    let send_text = eval_handshake_string(
        script_engine,
        &handshake.send,
        "handshake.send",
        frame_value,
        inputs,
        Some(&auth_json),
        cancel,
    )
    .map_err(fail)?;

    Ok(Message::Text(send_text.into()))
}

// -- main execute path --------------------------------------------------------

type AuthBinding = (String, Arc<dyn AuthStrategy>, Arc<ExecutionContext>);

/// Internal state accumulated while receiving frames.
struct SessionState {
    frames_received: u32,
    collected: Vec<Value>,
    sink: Option<FrameSink>,
    terminated_by: Option<TerminationReason>,
}

/// Abstraction over the `sink_file` output path so the receive loop is
/// agnostic about where a frame ultimately goes.
struct FrameSink {
    path: PathBuf,
    writer: tokio::io::BufWriter<tokio::fs::File>,
    bytes_written: u64,
}

impl FrameSink {
    async fn new(path: PathBuf, create_parents: bool) -> Result<Self, String> {
        if create_parents {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| format!("create parent dir '{}': {e}", parent.display()))?;
                }
            }
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .await
            .map_err(|e| format!("open '{}': {e}", path.display()))?;
        Ok(Self {
            path,
            writer: tokio::io::BufWriter::new(file),
            bytes_written: 0,
        })
    }

    async fn write_frame(&mut self, frame_json: &serde_json::Value) -> Result<(), String> {
        let mut line =
            serde_json::to_string(frame_json).map_err(|e| format!("serialize frame: {e}"))?;
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write frame: {e}"))?;
        self.bytes_written += line.len() as u64;
        Ok(())
    }

    async fn finalize(mut self) -> Result<(PathBuf, u64), String> {
        self.writer
            .flush()
            .await
            .map_err(|e| format!("flush sink: {e}"))?;
        self.writer
            .into_inner()
            .sync_all()
            .await
            .map_err(|e| format!("sync sink: {e}"))?;
        Ok((self.path, self.bytes_written))
    }
}

impl NodeHandler for WebSocketHandler {
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
        let script_engine = self.script_engine.clone();
        let predicate_asts = self.predicate_asts.clone();

        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(NodeError::Cancelled {
                    reason: "cancelled before WS handshake".into(),
                });
            }

            let cfg = WebSocketConfig::from_json(&config).map_err(|e| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': {e}"),
                recoverable: false,
            })?;

            // Resolve url early.
            let url_str = interpolate(&cfg.url_template, &inputs);
            let parsed_url = reqwest::Url::parse(&url_str).map_err(|e| NodeError::Failed {
                source_message: Some(e.to_string()),
                message: format!("node '{node_id}': invalid WS URL '{url_str}': {e}"),
                recoverable: false,
            })?;

            // Resolve sink path early for overwrite short-circuit.
            let sink_plan: Option<(PathBuf, bool, SinkOverwrite)> = match &cfg.emit {
                StreamEmitMode::SinkFile {
                    path_template,
                    create_parents,
                    overwrite,
                } => {
                    let p = PathBuf::from(interpolate(path_template, &inputs));
                    Some((p, *create_parents, *overwrite))
                }
                StreamEmitMode::Collect => None,
            };
            if let Some((path, _, overwrite)) = sink_plan.as_ref() {
                let exists = tokio::fs::metadata(path).await.is_ok();
                match overwrite {
                    SinkOverwrite::IfMissing if exists => {
                        let mut outputs = Outputs::new();
                        outputs.insert("frames_received".into(), Value::I64(0));
                        outputs.insert("terminated_by".into(), Value::String("skipped".into()));
                        outputs.insert(
                            "path".into(),
                            Value::String(path.to_string_lossy().into_owned()),
                        );
                        outputs.insert("bytes_written".into(), Value::I64(0));
                        outputs.insert("skipped".into(), Value::Bool(true));
                        return Ok(outputs);
                    }
                    SinkOverwrite::Never if exists => {
                        return Err(NodeError::Failed {
                            source_message: None,
                            message: format!(
                                "node '{node_id}': emit.sink_file: file '{}' exists and overwrite=never",
                                path.display()
                            ),
                            recoverable: false,
                        });
                    }
                    _ => {}
                }
            }

            // Resolve auth binding if needed.
            let auth_binding: Option<AuthBinding> = if let Some(name) = &cfg.auth_name {
                let ctx = exec_ctx.as_ref().ok_or_else(|| NodeError::Failed {
                    source_message: None,
                    message: format!(
                        "node '{node_id}': config.auth='{name}' requires an execution-context-bound WebSocketHandler"
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
                if !strategy.supports_ws() {
                    return Err(NodeError::Failed {
                        source_message: None,
                        message: format!(
                            "node '{node_id}': auth strategy '{name}' (type {}) does not support WebSocket handshakes",
                            strategy.type_name()
                        ),
                        recoverable: false,
                    });
                }
                Some((name.clone(), strategy.clone(), ctx.clone()))
            } else {
                None
            };

            // Compile predicate once.
            let predicate_ast = if let Some(src) = &cfg.terminate.predicate {
                Some(
                    compile_predicate(&script_engine, &predicate_asts, src).map_err(|e| {
                        NodeError::Failed {
                            source_message: None,
                            message: format!("node '{node_id}': {e}"),
                            recoverable: false,
                        }
                    })?,
                )
            } else {
                None
            };

            // Compile the reactive handshake once (trigger predicate +
            // optional auth_request.body + send scripts).
            let handshake = if let Some(hs) = &cfg.handshake {
                let compile_err = |e: String| NodeError::Failed {
                    source_message: None,
                    message: format!("node '{node_id}': {e}"),
                    recoverable: false,
                };
                let trigger = compile_predicate(&script_engine, &predicate_asts, &hs.trigger)
                    .map_err(|e| compile_err(format!("handshake.trigger: {e}")))?;
                let auth_request = if let Some(ar) = &hs.auth_request {
                    let body = compile_script(
                        &script_engine,
                        &predicate_asts,
                        "handshake.auth_request.body",
                        &ar.body,
                    )
                    .map_err(compile_err)?;
                    Some(CompiledAuthRequest {
                        url_template: ar.url.clone(),
                        method: ar.method.clone(),
                        content_type: ar.content_type.clone(),
                        body,
                        auth: ar.auth.clone(),
                    })
                } else {
                    None
                };
                let send =
                    compile_script(&script_engine, &predicate_asts, "handshake.send", &hs.send)
                        .map_err(compile_err)?;
                Some(CompiledHandshake {
                    trigger,
                    auth_request,
                    send,
                })
            } else {
                None
            };

            // Compile validator once (shared across frames).
            let validator = if let Some(vjson) = cfg.validation_cfg.as_ref() {
                Some(
                    get_or_compile_validator(&validators, vjson, &inputs).map_err(|e| {
                        NodeError::Failed {
                            source_message: None,
                            message: format!("node '{node_id}': {e}"),
                            recoverable: false,
                        }
                    })?,
                )
            } else {
                None
            };

            // -- build handshake request -----------------------------------
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            let mut request =
                url_str
                    .as_str()
                    .into_client_request()
                    .map_err(|e| NodeError::Failed {
                        source_message: Some(e.to_string()),
                        message: format!("node '{node_id}': WS request build failed: {e}"),
                        recoverable: false,
                    })?;

            if let Some(sp) = cfg.subprotocol.as_deref() {
                let v = http::HeaderValue::try_from(sp).map_err(|e| NodeError::Failed {
                    source_message: Some(e.to_string()),
                    message: format!("node '{node_id}': invalid subprotocol '{sp}': {e}"),
                    recoverable: false,
                })?;
                request
                    .headers_mut()
                    .insert(http::header::SEC_WEBSOCKET_PROTOCOL, v);
            }

            // Apply auth.
            if let Some((name, strategy, ctx)) = auth_binding.as_ref() {
                request = apply_ws_auth(
                    &node_id,
                    ctx,
                    strategy.as_ref(),
                    name,
                    &inputs,
                    template.clone(),
                    &parsed_url,
                    request,
                )
                .await?;
            }

            // -- connect ---------------------------------------------------
            let connect_fut = tokio_tungstenite::connect_async(request);
            let (ws_stream, _resp) = tokio::select! {
                r = connect_fut => r.map_err(|e| NodeError::Failed {
                    source_message: Some(e.to_string()),
                    message: format!("node '{node_id}': WS connect failed: {e}"),
                    recoverable: false,
                })?,
                _ = cancel.cancelled() => {
                    return Err(NodeError::Cancelled {
                        reason: "cancelled during WS connect".into(),
                    });
                }
            };

            let (mut writer, mut reader) = ws_stream.split();

            // Send init frames.
            for (i, frame) in cfg.init_frames.iter().enumerate() {
                let msg = match frame {
                    WsFrame::Text(t) => Message::Text(interpolate(t, &inputs).into()),
                    WsFrame::Binary(b) => Message::Binary(b.clone().into()),
                };
                tokio::select! {
                    r = writer.send(msg) => r.map_err(|e| NodeError::Failed {
                        source_message: Some(e.to_string()),
                        message: format!(
                            "node '{node_id}': WS init_frames[{i}] send failed: {e}"
                        ),
                        recoverable: false,
                    })?,
                    _ = cancel.cancelled() => {
                        return Err(NodeError::Cancelled {
                            reason: "cancelled while sending init frames".into(),
                        });
                    }
                }
            }

            // -- receive loop ----------------------------------------------
            let mut state = SessionState {
                frames_received: 0,
                collected: Vec::new(),
                sink: None,
                terminated_by: None,
            };
            if let Some((path, create_parents, _)) = sink_plan.clone() {
                let sink =
                    FrameSink::new(path, create_parents)
                        .await
                        .map_err(|e| NodeError::Failed {
                            source_message: None,
                            message: format!("node '{node_id}': emit.sink_file: {e}"),
                            recoverable: false,
                        })?;
                state.sink = Some(sink);
            }

            let deadline = cfg
                .terminate
                .timeout_ms
                .map(|ms| tokio::time::Instant::now() + Duration::from_millis(ms));
            let max_frames = cfg.terminate.max_frames;

            // Reactive-handshake state: fires at most once.
            let mut handshake_completed = false;

            loop {
                // Build a single future to wait on next event.
                // Three-way select: cancellation, timeout, next-frame.
                let next = async { reader.next().await };
                let timeout_fut = async {
                    match deadline {
                        Some(d) => {
                            tokio::time::sleep_until(d).await;
                            true
                        }
                        None => {
                            std::future::pending::<()>().await;
                            false
                        }
                    }
                };

                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        state.terminated_by = Some(TerminationReason::Cancelled);
                        break;
                    }
                    did_fire = timeout_fut => {
                        if did_fire {
                            state.terminated_by = Some(TerminationReason::Timeout);
                            break;
                        }
                    }
                    next_msg = next => {
                        match next_msg {
                            None => {
                                // Stream ended without a close frame.
                                state.terminated_by = Some(TerminationReason::ServerClose);
                                break;
                            }
                            Some(Err(e)) => {
                                return Err(NodeError::Failed {
                                    source_message: Some(e.to_string()),
                                    message: format!("node '{node_id}': WS read error: {e}"),
                                    recoverable: false,
                                });
                            }
                            Some(Ok(msg)) => {
                                match msg {
                                    Message::Text(_) | Message::Binary(_) => {
                                        // Reactive handshake: on a triggering
                                        // frame, consume it (no count / collect
                                        // / validate / terminate) and reply with
                                        // a computed frame. Fires at most once.
                                        if let Some(hs) = handshake.as_ref() {
                                            if !handshake_completed {
                                                let frame_value = decode_frame_value(&msg);
                                                let triggered = eval_handshake_trigger(
                                                    &script_engine,
                                                    &hs.trigger,
                                                    &frame_value,
                                                    &cancel,
                                                )
                                                .map_err(|e| NodeError::Failed {
                                                    source_message: None,
                                                    message: format!("node '{node_id}': {e}"),
                                                    recoverable: false,
                                                })?;
                                                if triggered {
                                                    let reply = run_handshake(
                                                        &node_id,
                                                        hs,
                                                        &frame_value,
                                                        &inputs,
                                                        &exec_ctx,
                                                        &template,
                                                        &script_engine,
                                                        &cancel,
                                                    )
                                                    .await?;
                                                    writer.send(reply).await.map_err(|e| {
                                                        NodeError::Failed {
                                                            source_message: Some(e.to_string()),
                                                            message: format!(
                                                                "node '{node_id}': handshake send failed: {e}"
                                                            ),
                                                            recoverable: false,
                                                        }
                                                    })?;
                                                    handshake_completed = true;
                                                    continue;
                                                }
                                            }
                                        }
                                        let outcome = handle_data_frame(
                                            &node_id,
                                            &msg,
                                            state.frames_received,
                                            validator.as_ref(),
                                            predicate_ast.as_deref(),
                                            &script_engine,
                                            &cancel,
                                            &mut state,
                                        )
                                        .await?;
                                        match outcome {
                                            FrameOutcome::Continue => {}
                                            FrameOutcome::TerminatedBy(r) => {
                                                state.terminated_by = Some(r);
                                                break;
                                            }
                                        }
                                        if let Some(cap) = max_frames {
                                            if state.frames_received >= cap {
                                                state.terminated_by = Some(TerminationReason::MaxFrames);
                                                break;
                                            }
                                        }
                                    }
                                    Message::Close(_) => {
                                        state.terminated_by = Some(TerminationReason::ServerClose);
                                        break;
                                    }
                                    // Ping/Pong/Frame are transport-level — ignore.
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }

            // Best-effort close.
            if cfg.terminate.close_on_terminate {
                let close = Message::Close(Some(CloseFrame {
                    code: CloseCode::Normal,
                    reason: "".into(),
                }));
                let _ = writer.send(close).await;
                let _ = writer.close().await;
            }

            // -- build outputs ---------------------------------------------
            let reason = state
                .terminated_by
                .unwrap_or(TerminationReason::ServerClose);
            let mut outputs = Outputs::new();
            outputs.insert(
                "frames_received".into(),
                Value::I64(state.frames_received as i64),
            );
            outputs.insert(
                "terminated_by".into(),
                Value::String(reason.as_str().into()),
            );

            if let Some(sink) = state.sink {
                let (path, bytes_written) =
                    sink.finalize().await.map_err(|e| NodeError::Failed {
                        source_message: None,
                        message: format!("node '{node_id}': emit.sink_file finalize: {e}"),
                        recoverable: false,
                    })?;
                outputs.insert(
                    "path".into(),
                    Value::String(path.to_string_lossy().into_owned()),
                );
                outputs.insert("bytes_written".into(), Value::I64(bytes_written as i64));
            } else {
                outputs.insert("frames".into(), Value::Vec(state.collected));
            }

            Ok(outputs)
        })
    }

    fn validate_node(
        &self,
        node: &Node,
        _graph: &Graph,
        _ctx: &ExecutionContext,
    ) -> Result<(), Vec<ValidationIssue>> {
        let mut issues = Vec::new();

        // Surface config-shape errors first.
        let cfg = match WebSocketConfig::from_json(&node.config) {
            Ok(c) => c,
            Err(e) => {
                issues.push(ValidationIssue::new(
                    String::new(),
                    String::new(),
                    ValidationIssueKind::Config,
                    e,
                ));
                return Err(issues);
            }
        };

        // Pre-compile the termination predicate so Rhai syntax errors fire
        // before any network hop. Uses the same cache the runtime path
        // reads, so `execute()` does not recompile.
        if let Some(src) = &cfg.terminate.predicate {
            if let Err(e) = compile_predicate(&self.script_engine, &self.predicate_asts, src) {
                issues.push(ValidationIssue::new(
                    String::new(),
                    String::new(),
                    ValidationIssueKind::ScriptCompile,
                    e,
                ));
            }
        }

        // Pre-compile handshake scripts so Rhai syntax errors surface at load
        // time. Same shared cache as the runtime path.
        if let Some(hs) = &cfg.handshake {
            let mut push = |res: Result<Arc<AST>, String>| {
                if let Err(e) = res {
                    issues.push(ValidationIssue::new(
                        String::new(),
                        String::new(),
                        ValidationIssueKind::ScriptCompile,
                        e,
                    ));
                }
            };
            push(compile_predicate(
                &self.script_engine,
                &self.predicate_asts,
                &hs.trigger,
            ));
            if let Some(ar) = &hs.auth_request {
                push(compile_script(
                    &self.script_engine,
                    &self.predicate_asts,
                    "handshake.auth_request.body",
                    &ar.body,
                ));
            }
            push(compile_script(
                &self.script_engine,
                &self.predicate_asts,
                "handshake.send",
                &hs.send,
            ));
        }

        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    fn schema(&self, name: &str) -> HandlerSchema {
        HandlerSchema::new(name, "Open a WebSocket connection, optionally send init frames, and stream received frames until a termination trigger fires.")
            .with_config(
                SchemaField::new("url", "string")
                    .required()
                    .describe("WS URL template with {key} interpolation (ws:// or wss://)"),
            )
            .with_config(
                SchemaField::new("auth", "string")
                    .describe("Name of a graph-scoped auth strategy. The strategy must support WS."),
            )
            .with_config(SchemaField::new("init_frames", "array").describe(
                "Init frames to send after connect. Each entry is either a string (sent as text) \
                 or { text: \"...\" } | { binary: \"...\" } | { binary_bytes: [u8, ...] }",
            ))
            .with_config(SchemaField::new("subprotocol", "string").describe(
                "Optional Sec-WebSocket-Protocol value set on the handshake.",
            ))
            .with_config(SchemaField::new("terminate", "object").describe(
                "{ on_predicate?: <rhai-expr over `frame` + `frame_index`>, max_frames?: u32, \
                 timeout_ms?: u64, close_on_terminate?: bool (default true) }",
            ))
            .with_config(SchemaField::new("handshake", "object").describe(
                "Reactive subscribe handshake: { trigger: <rhai-expr over `frame`>, \
                 auth_request?: { url, method?, content_type?, body: <rhai over `frame`,`inputs`>, auth? }, \
                 send: <rhai over `frame`,`inputs`,`auth`> }. On the first frame matching `trigger`, \
                 optionally POSTs to the auth endpoint then sends the computed `send` frame; the \
                 triggering frame is consumed (not counted/collected/validated).",
            ))
            .with_config(SchemaField::new("validation", "object").describe(
                "{ inline | file, on_failure: fail|passthrough } — JSON Schema validation applied \
                 to each received text frame (parsed as JSON).",
            ))
            .with_config(SchemaField::new("emit", "string|object").describe(
                "\"collect\" (default) accumulates frames into outputs.frames; \
                 { sink_file: { path, overwrite?, create_parents? } } streams JSON-per-line to disk.",
            ))
            .with_output(SchemaField::new("frames_received", "integer"))
            .with_output(
                SchemaField::new("terminated_by", "string")
                    .describe("predicate | max_frames | timeout | cancelled | server_close | validation_error"),
            )
            .with_output(
                SchemaField::new("frames", "array")
                    .describe("emit=collect: array of { kind, text?, binary_bytes?, json?, validation_error? } objects"),
            )
            .with_output(
                SchemaField::new("path", "string").describe("emit=sink_file: final on-disk path"),
            )
            .with_output(
                SchemaField::new("bytes_written", "integer")
                    .describe("emit=sink_file: bytes streamed to disk"),
            )
    }
}

/// Result of handling a single data frame in the receive loop.
enum FrameOutcome {
    Continue,
    TerminatedBy(TerminationReason),
}

/// Decode a data `Message` into the `frame` scope value used by the reactive
/// handshake scripts. Per the handshake contract, `frame` is the *parsed JSON*
/// (a Map) when the text frame is valid JSON, or the raw text/bytes otherwise
/// — distinct from the terminate predicate's `{ kind, text, json }` wrapper.
/// Decode a WS message into the same `{kind, text, json}` shape the terminate
/// predicate and collected output use, so handshake scripts address fields the
/// same way the rest of the handler does: `frame.json.<field>` (and `frame.text`
/// for the raw payload). Keeping one convention avoids the easy-to-miss trap of
/// `frame.event` (works) vs `frame.json.event` (works elsewhere).
fn decode_frame_value(msg: &Message) -> Value {
    match msg {
        Message::Text(t) => {
            let s = t.as_str();
            let parsed = serde_json::from_str::<serde_json::Value>(s).ok();
            frame_to_value("text", Some(s), None, parsed.as_ref(), None)
        }
        Message::Binary(b) => frame_to_value("binary", None, Some(b.as_ref()), None, None),
        _ => Value::Null,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_data_frame(
    node_id: &str,
    msg: &Message,
    frame_index_before_incr: u32,
    validator: Option<&CompiledValidator>,
    predicate_ast: Option<&AST>,
    script_engine: &ScriptEngine,
    cancel: &CancellationToken,
    state: &mut SessionState,
) -> Result<FrameOutcome, NodeError> {
    // Decode the frame payload up front so every downstream path (validation,
    // predicate, emit) shares one parse.
    let (kind, raw_text_opt, raw_bytes_opt, parsed_json, parse_error) = match msg {
        Message::Text(t) => {
            let s = t.as_str();
            match serde_json::from_str::<serde_json::Value>(s) {
                Ok(j) => ("text", Some(s), None, Some(j), None),
                Err(e) => ("text", Some(s), None, None, Some(e.to_string())),
            }
        }
        Message::Binary(b) => ("binary", None, Some(b.as_ref()), None, None),
        _ => unreachable!("non-data messages filtered before handle_data_frame"),
    };

    // Validation.
    let mut validation_error_json: Option<serde_json::Value> = None;
    if let Some(v) = validator {
        let report = run_frame_validation(v, parsed_json.as_ref(), parse_error.clone());
        match (v.failure_mode(), report) {
            (_, FrameValidationReport::Valid) => {}
            (FailureMode::Fail, FrameValidationReport::Invalid { errors }) => {
                let detail = serde_json::to_string(&errors).unwrap_or_else(|_| "[]".into());
                return Err(NodeError::Failed {
                    source_message: Some(detail.clone()),
                    message: format!(
                        "node '{node_id}': WS frame schema validation failed: {detail}"
                    ),
                    recoverable: false,
                });
            }
            (FailureMode::Fail, FrameValidationReport::NotJson { reason }) => {
                return Err(NodeError::Failed {
                    source_message: Some(reason.clone()),
                    message: format!(
                        "node '{node_id}': WS frame not JSON (validation fail mode): {reason}"
                    ),
                    recoverable: false,
                });
            }
            (FailureMode::Passthrough, FrameValidationReport::Invalid { errors }) => {
                validation_error_json =
                    Some(serde_json::to_value(&errors).unwrap_or(serde_json::Value::Null));
            }
            (FailureMode::Passthrough, FrameValidationReport::NotJson { reason }) => {
                validation_error_json = Some(serde_json::json!({
                    "kind": "not_json",
                    "reason": reason,
                }));
            }
        }
    }

    // Build the frame's Value representation (shared by predicate + emit).
    let frame_value = frame_to_value(
        kind,
        raw_text_opt,
        raw_bytes_opt,
        parsed_json.as_ref(),
        validation_error_json.as_ref(),
    );

    // Emit into sink or collection BEFORE predicate evaluation so the
    // predicate-terminating frame is also reflected in output. Counter
    // increments too.
    let emit_json = match &frame_value {
        Value::Map(_) => value_to_json(&frame_value),
        _ => value_to_json(&frame_value),
    };
    state.frames_received = frame_index_before_incr + 1;
    if let Some(sink) = state.sink.as_mut() {
        sink.write_frame(&emit_json)
            .await
            .map_err(|e| NodeError::Failed {
                source_message: None,
                message: format!("node '{node_id}': emit.sink_file: {e}"),
                recoverable: false,
            })?;
    } else {
        state.collected.push(frame_value.clone());
    }

    // Predicate evaluation.
    if let Some(ast) = predicate_ast {
        let matched = eval_predicate(
            script_engine,
            ast,
            &frame_value,
            state.frames_received,
            cancel,
        )
        .map_err(|e| NodeError::Failed {
            source_message: None,
            message: format!("node '{node_id}': {e}"),
            recoverable: false,
        })?;
        if matched {
            return Ok(FrameOutcome::TerminatedBy(TerminationReason::Predicate));
        }
    }

    Ok(FrameOutcome::Continue)
}

/// Convert the graph-side frame Value into a JSON-friendly form for sink
/// serialisation.
fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::I64(n) => serde_json::Value::Number((*n).into()),
        Value::F32(f) => serde_json::Number::from_f64(*f as f64)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Vec(items) => serde_json::Value::Array(items.iter().map(value_to_json).collect()),
        Value::Map(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
        Value::Domain { data, .. } => data.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validate_node_flags_bad_predicate() {
        let h = WebSocketHandler::stateless();
        let mut node = Node::new("W", "Ws");
        node.config = json!({
            "url": "wss://example.com",
            "terminate": { "on_predicate": "let x = ;; garbage" }
        });
        let graph = Graph::new();
        let ctx = ExecutionContext::new();
        let issues = h.validate_node(&node, &graph, &ctx).unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, ValidationIssueKind::ScriptCompile);
    }

    #[test]
    fn validate_node_flags_bad_config_shape() {
        let h = WebSocketHandler::stateless();
        // missing config.url
        let mut node = Node::new("W", "Ws");
        node.config = json!({});
        let graph = Graph::new();
        let ctx = ExecutionContext::new();
        let issues = h.validate_node(&node, &graph, &ctx).unwrap_err();
        assert_eq!(issues[0].kind, ValidationIssueKind::Config);
    }

    #[test]
    fn validate_node_caches_predicate_for_runtime() {
        let h = WebSocketHandler::stateless();
        let mut node = Node::new("W", "Ws");
        node.config = json!({
            "url": "wss://example.com",
            "terminate": { "on_predicate": "frame_index > 3" }
        });
        let graph = Graph::new();
        let ctx = ExecutionContext::new();
        h.validate_node(&node, &graph, &ctx).unwrap();
        // Runtime reads from the same cache.
        assert_eq!(h.predicate_asts.lock().unwrap().len(), 1);
    }

    #[test]
    fn parse_init_frames_string_variants() {
        let v = json!([
            "hello",
            { "text": "world" },
            { "binary": "bytes" },
            { "binary_bytes": [1, 2, 3] }
        ]);
        let frames = parse_init_frames(Some(&v)).unwrap();
        assert_eq!(frames.len(), 4);
        assert!(matches!(frames[0], WsFrame::Text(ref s) if s == "hello"));
        assert!(matches!(frames[1], WsFrame::Text(ref s) if s == "world"));
        assert!(matches!(frames[2], WsFrame::Binary(ref b) if b == b"bytes"));
        assert!(matches!(frames[3], WsFrame::Binary(ref b) if b == &vec![1u8, 2, 3]));
    }

    #[test]
    fn parse_init_frames_rejects_non_array() {
        assert!(parse_init_frames(Some(&json!("not-array"))).is_err());
    }

    #[test]
    fn parse_init_frames_rejects_empty_object() {
        let v = json!([{}]);
        assert!(parse_init_frames(Some(&v)).is_err());
    }

    #[test]
    fn parse_init_frames_rejects_out_of_range_byte() {
        let v = json!([{ "binary_bytes": [1, 300] }]);
        assert!(parse_init_frames(Some(&v)).is_err());
    }

    #[test]
    fn parse_emit_default_is_collect() {
        assert!(matches!(parse_emit(None).unwrap(), StreamEmitMode::Collect));
    }

    #[test]
    fn parse_emit_string_collect() {
        assert!(matches!(
            parse_emit(Some(&json!("collect"))).unwrap(),
            StreamEmitMode::Collect
        ));
    }

    #[test]
    fn parse_emit_sink_file_requires_path() {
        let v = json!({ "sink_file": {} });
        assert!(parse_emit(Some(&v)).is_err());
    }

    #[test]
    fn parse_emit_sink_file_full() {
        let v = json!({
            "sink_file": {
                "path": "/tmp/x.ndjson",
                "create_parents": false,
                "overwrite": "never"
            }
        });
        match parse_emit(Some(&v)).unwrap() {
            StreamEmitMode::SinkFile {
                path_template,
                create_parents,
                overwrite,
            } => {
                assert_eq!(path_template, "/tmp/x.ndjson");
                assert!(!create_parents);
                assert_eq!(overwrite, SinkOverwrite::Never);
            }
            other => panic!("unexpected emit: {other:?}"),
        }
    }

    #[test]
    fn parse_emit_rejects_unknown_string() {
        assert!(parse_emit(Some(&json!("bogus"))).is_err());
    }

    #[test]
    fn parse_terminate_all_fields() {
        let v = json!({
            "on_predicate": "frame.json.done == true",
            "max_frames": 5,
            "timeout_ms": 1000,
            "close_on_terminate": false
        });
        let t = TerminateCfg::from_config(Some(&v)).unwrap();
        assert_eq!(t.predicate.as_deref(), Some("frame.json.done == true"));
        assert_eq!(t.max_frames, Some(5));
        assert_eq!(t.timeout_ms, Some(1000));
        assert!(!t.close_on_terminate);
    }

    #[test]
    fn parse_terminate_defaults() {
        let t = TerminateCfg::from_config(None).unwrap();
        assert!(t.predicate.is_none());
        assert!(t.max_frames.is_none());
        assert!(t.timeout_ms.is_none());
        assert!(t.close_on_terminate);
    }

    #[test]
    fn websocket_config_missing_url() {
        match WebSocketConfig::from_json(&json!({})) {
            Err(e) => assert!(e.contains("missing config.url")),
            Ok(_) => panic!("expected missing-url error"),
        }
    }

    #[test]
    fn websocket_config_full_roundtrip() {
        let cfg = match WebSocketConfig::from_json(&json!({
            "url": "ws://localhost:1234/",
            "auth": "api",
            "init_frames": ["hi"],
            "subprotocol": "my.sub",
            "terminate": { "max_frames": 3 },
            "validation": { "inline": { "type": "object" } },
            "emit": "collect"
        })) {
            Ok(c) => c,
            Err(e) => panic!("unexpected error: {e}"),
        };
        assert_eq!(cfg.url_template, "ws://localhost:1234/");
        assert_eq!(cfg.auth_name.as_deref(), Some("api"));
        assert_eq!(cfg.init_frames.len(), 1);
        assert_eq!(cfg.terminate.max_frames, Some(3));
        assert!(cfg.validation_cfg.is_some());
        assert_eq!(cfg.subprotocol.as_deref(), Some("my.sub"));
    }

    #[tokio::test]
    async fn missing_url_errors() {
        let node = Node::new("W", "WS");
        let result = WebSocketHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing config.url"));
    }

    #[tokio::test]
    async fn cancellation_before_connect() {
        let mut node = Node::new("W", "WS");
        node.config = json!({ "url": "ws://localhost:9/" });
        let token = CancellationToken::new();
        token.cancel();
        let err = WebSocketHandler::stateless()
            .execute(&node, Outputs::new(), token)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cancelled"));
    }

    // -- handshake config parsing --------------------------------------------

    #[test]
    fn parse_handshake_full_valid() {
        let v = json!({
            "trigger": "frame.json.event == \"connection_established\"",
            "auth_request": {
                "url": "http://127.0.0.1:1/auth",
                "method": "POST",
                "content_type": "application/x-www-form-urlencoded",
                "body": "\"socket_id=x\"",
                "auth": "api"
            },
            "send": "\"subscribe\""
        });
        let h = HandshakeCfg::from_json(Some(&v)).unwrap().unwrap();
        assert_eq!(h.trigger, "frame.json.event == \"connection_established\"");
        let ar = h.auth_request.as_ref().unwrap();
        assert_eq!(ar.url, "http://127.0.0.1:1/auth");
        assert_eq!(ar.method, "POST");
        assert_eq!(ar.content_type, "application/x-www-form-urlencoded");
        assert_eq!(ar.auth.as_deref(), Some("api"));
    }

    #[test]
    fn parse_handshake_defaults_method_and_content_type() {
        let v = json!({
            "trigger": "true",
            "auth_request": { "url": "http://x/", "body": "\"b\"" },
            "send": "\"s\""
        });
        let h = HandshakeCfg::from_json(Some(&v)).unwrap().unwrap();
        let ar = h.auth_request.as_ref().unwrap();
        assert_eq!(ar.method, "POST");
        assert_eq!(ar.content_type, "application/x-www-form-urlencoded");
        assert!(ar.auth.is_none());
    }

    #[test]
    fn parse_handshake_no_auth_request_is_public() {
        let v = json!({ "trigger": "true", "send": "\"s\"" });
        let h = HandshakeCfg::from_json(Some(&v)).unwrap().unwrap();
        assert!(h.auth_request.is_none());
    }

    #[test]
    fn parse_handshake_absent_is_none() {
        assert!(HandshakeCfg::from_json(None).unwrap().is_none());
    }

    #[test]
    fn parse_handshake_missing_trigger_errors() {
        let v = json!({ "send": "\"s\"" });
        assert!(HandshakeCfg::from_json(Some(&v)).is_err());
    }

    #[test]
    fn parse_handshake_missing_send_errors() {
        let v = json!({ "trigger": "true" });
        assert!(HandshakeCfg::from_json(Some(&v)).is_err());
    }

    #[test]
    fn parse_handshake_auth_request_missing_url_errors() {
        let v = json!({
            "trigger": "true",
            "auth_request": { "body": "\"b\"" },
            "send": "\"s\""
        });
        assert!(HandshakeCfg::from_json(Some(&v)).is_err());
    }

    #[test]
    fn parse_handshake_auth_request_missing_body_errors() {
        let v = json!({
            "trigger": "true",
            "auth_request": { "url": "http://x/" },
            "send": "\"s\""
        });
        assert!(HandshakeCfg::from_json(Some(&v)).is_err());
    }

    #[test]
    fn parse_handshake_not_object_errors() {
        assert!(HandshakeCfg::from_json(Some(&json!("nope"))).is_err());
    }

    #[test]
    fn websocket_config_parses_handshake_field() {
        let cfg = WebSocketConfig::from_json(&json!({
            "url": "ws://localhost/",
            "handshake": {
                "trigger": "frame.json.event == \"connection_established\"",
                "send": "\"sub\""
            }
        }))
        .unwrap();
        assert!(cfg.handshake.is_some());
    }

    // -- reactive handshake integration --------------------------------------

    /// Mock auth HTTP server: accepts one connection, reads the request, and
    /// returns a fixed JSON body `{"auth":"key:sig"}`. Returns the auth URL.
    async fn spawn_mock_auth_server() -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _peer) = listener.accept().await.unwrap();
            // Read the request (headers + small body) — enough to capture it.
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            buf.truncate(n);
            let body = br#"{"auth":"key:sig"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            sock.write_all(body).await.unwrap();
            sock.flush().await.unwrap();
            buf
        });
        (format!("http://127.0.0.1:{port}/auth"), handle)
    }

    /// Mock WS server: on connect, sends a Pusher-style
    /// `connection_established` frame, reads exactly one client frame (the
    /// subscribe), stores it, sends a `done` frame, then waits for close.
    /// Returns the captured subscribe-frame text.
    async fn spawn_mock_ws_server() -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let (stream, _peer) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            // (a) connection_established: `data` is a JSON *string*.
            let established = serde_json::json!({
                "event": "connection_established",
                "data": "{\"socket_id\":\"123.456\"}"
            })
            .to_string();
            ws.send(Message::Text(established.into())).await.unwrap();
            // (b) read exactly one client frame (the subscribe).
            let mut captured = String::new();
            while let Some(msg) = ws.next().await {
                match msg {
                    Ok(Message::Text(t)) => {
                        captured = t.to_string();
                        break;
                    }
                    Ok(Message::Close(_)) | Err(_) => break,
                    _ => {}
                }
            }
            // (c) done frame.
            ws.send(Message::Text(r#"{"event":"done"}"#.into()))
                .await
                .unwrap();
            // Wait for the client's close.
            while let Some(msg) = ws.next().await {
                if matches!(msg, Ok(Message::Close(_)) | Err(_)) {
                    break;
                }
            }
            captured
        });
        (format!("ws://127.0.0.1:{port}/"), handle)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reactive_handshake_subscribes_then_continues() {
        let (auth_url, auth_handle) = spawn_mock_auth_server().await;
        let (ws_url, ws_handle) = spawn_mock_ws_server().await;

        let mut node = Node::new("W", "WS");
        node.config = json!({
            "url": ws_url,
            "handshake": {
                "trigger": "frame.json.event == \"connection_established\"",
                "auth_request": {
                    "url": auth_url,
                    // `frame.json.data` is itself a JSON *string*; parse it in rhai.
                    "body": "let d = parse_json(frame.json.data); \"socket_id=\" + d.socket_id + \"&channel_name=private-test\"",
                },
                "send": "\"{\\\"event\\\":\\\"pusher:subscribe\\\",\\\"data\\\":{\\\"auth\\\":\\\"\" + auth.auth + \"\\\",\\\"channel\\\":\\\"private-test\\\"}}\""
            },
            "terminate": { "on_predicate": "frame.json.event == \"done\"" }
        });

        let out = WebSocketHandler::stateless()
            .execute(&node, Outputs::new(), CancellationToken::new())
            .await
            .expect("session runs");

        let captured_subscribe = ws_handle.await.unwrap();
        let captured_auth_req = auth_handle.await.unwrap();
        let auth_req_text = String::from_utf8_lossy(&captured_auth_req);

        // The auth POST carried the parsed socket_id + channel.
        assert!(
            auth_req_text.contains("socket_id=123.456"),
            "auth request body missing socket_id: {auth_req_text}"
        );
        assert!(
            auth_req_text.contains("channel_name=private-test"),
            "auth request body missing channel_name: {auth_req_text}"
        );

        // The server-captured subscribe frame carries the auth signature and
        // channel from the computed `send` script.
        assert!(
            captured_subscribe.contains("key:sig"),
            "subscribe frame missing auth sig: {captured_subscribe}"
        );
        assert!(
            captured_subscribe.contains("private-test"),
            "subscribe frame missing channel: {captured_subscribe}"
        );

        // Terminated via the predicate on the `done` frame.
        assert_eq!(
            match out.get("terminated_by").unwrap() {
                Value::String(s) => s.as_str(),
                other => panic!("terminated_by: {other:?}"),
            },
            "predicate"
        );

        // The connection_established frame was consumed by the handshake — it
        // does NOT appear in collected output and does NOT count toward
        // frames_received. Only the `done` frame is collected.
        let frames = match out.get("frames").unwrap() {
            Value::Vec(v) => v.clone(),
            other => panic!("frames: {other:?}"),
        };
        assert_eq!(frames.len(), 1, "only the done frame should be collected");
        assert_eq!(
            match out.get("frames_received").unwrap() {
                Value::I64(n) => *n,
                other => panic!("frames_received: {other:?}"),
            },
            1
        );
        let done = match &frames[0] {
            Value::Map(m) => m,
            other => panic!("frame: {other:?}"),
        };
        let done_json = done.get("json").expect("done frame parsed json");
        match done_json {
            Value::Map(m) => assert_eq!(m.get("event"), Some(&Value::String("done".into()))),
            other => panic!("done json: {other:?}"),
        }
        // No connection_established frame leaked into the collection.
        for f in &frames {
            if let Value::Map(m) = f {
                if let Some(Value::Map(j)) = m.get("json") {
                    assert_ne!(
                        j.get("event"),
                        Some(&Value::String("connection_established".into())),
                        "connection_established leaked into output"
                    );
                }
            }
        }
    }
}
