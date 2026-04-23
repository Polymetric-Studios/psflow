//! Transport-agnostic JSON Schema validation.
//!
//! This module compiles a JSON Schema once (per handler construction) and
//! validates `serde_json::Value` instances against it. Designed to be reused
//! by both the HTTP handler (for response bodies) and the eventual WebSocket
//! handler (for per-frame validation) — it has no transport dependencies.
//!
//! ## Shape
//!
//! - [`ValidationConfig`] — deserializable config surface: schema source
//!   (`inline` or `file`) + failure mode (`fail` / `passthrough`).
//! - [`CompiledValidator`] — wraps a compiled `jsonschema::Validator` plus
//!   the configured failure mode. Cheap to clone via `Arc`.
//! - [`ValidationOutcome`] — `Valid` or `Invalid { errors }`. Consumers
//!   translate per-failure-mode into either a `NodeError` or an output
//!   field named `validation_error`.
//!
//! ## Schema sources
//!
//! - `Inline(serde_json::Value)` — embedded in config.
//! - `File { path }` — path template interpolated at construction time from
//!   the caller-provided substitution map, then loaded from disk. Schemas
//!   are compiled once at handler construction — not per request.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

// Note: combining validation with `body_sink` (streaming to disk) is
// rejected at HTTP handler config-parse time — validation needs the parsed
// body, body_sink streams past it. See `handlers/http.rs`.

/// How a validation failure is reported.
///
/// - `Fail` (default): validation failure fails the node with a structured
///   error naming the failing paths + keywords.
/// - `Passthrough`: the node succeeds, and output gains a `validation_error`
///   field with the failure details; `validation_ok` is set to `false` on
///   failure and `true` on success.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureMode {
    #[default]
    Fail,
    Passthrough,
}

/// Where to load the schema from.
///
/// Exactly one of `inline` or `file` must be set. Mixing is a config error.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationConfig {
    /// Inline JSON Schema document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline: Option<serde_json::Value>,
    /// Filesystem path template for the schema document. The caller is
    /// responsible for interpolating `{key}` placeholders before
    /// compilation — this config surface carries the raw template.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Failure mode. Defaults to `fail`.
    #[serde(default)]
    pub on_failure: FailureMode,
}

impl ValidationConfig {
    /// Parse a validation config from a `serde_json::Value` — the shape
    /// carried in `node.config["validation"]`.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, ValidationConfigError> {
        let cfg: ValidationConfig =
            serde_json::from_value(v.clone()).map_err(|e| ValidationConfigError::Invalid {
                message: e.to_string(),
            })?;
        cfg.validate_exclusivity()?;
        Ok(cfg)
    }

    fn validate_exclusivity(&self) -> Result<(), ValidationConfigError> {
        match (self.inline.is_some(), self.file.is_some()) {
            (true, true) => Err(ValidationConfigError::Invalid {
                message: "validation: set exactly one of `inline` or `file`, not both".into(),
            }),
            (false, false) => Err(ValidationConfigError::Invalid {
                message: "validation: one of `inline` or `file` is required".into(),
            }),
            _ => Ok(()),
        }
    }
}

/// Errors from parsing / compiling / loading a validation config.
#[derive(Debug)]
pub enum ValidationConfigError {
    /// Config shape is wrong (serde failure, missing / conflicting source).
    Invalid { message: String },
    /// File-backed schema failed to read from disk.
    FileRead {
        path: PathBuf,
        source: std::io::Error,
    },
    /// File contents were not valid JSON.
    FileParse {
        path: PathBuf,
        source: serde_json::Error,
    },
    /// The schema itself failed to compile (invalid JSON Schema).
    Compile { message: String },
}

impl std::fmt::Display for ValidationConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid { message } => write!(f, "invalid validation config: {message}"),
            Self::FileRead { path, source } => {
                write!(
                    f,
                    "failed to read schema file '{}': {source}",
                    path.display()
                )
            }
            Self::FileParse { path, source } => write!(
                f,
                "schema file '{}' is not valid JSON: {source}",
                path.display()
            ),
            Self::Compile { message } => write!(f, "schema compile error: {message}"),
        }
    }
}

impl std::error::Error for ValidationConfigError {}

/// A single validation failure — JSON-pointer path, keyword, and message.
///
/// Exposed in both `fail`-mode error strings (serialised as JSON) and
/// `passthrough` mode (attached to the `validation_error` output field).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ValidationFailure {
    /// JSON pointer into the instance where validation failed (e.g.
    /// `/items/0/name`). Empty string means the root.
    pub instance_path: String,
    /// JSON pointer into the schema for the failing keyword.
    pub schema_path: String,
    /// Name of the failing keyword (e.g. `required`, `type`, `minLength`).
    pub keyword: String,
    /// Human-readable failure message from `jsonschema`.
    pub message: String,
}

/// Result of a validation pass.
#[derive(Debug, Clone)]
pub enum ValidationOutcome {
    Valid,
    Invalid { errors: Vec<ValidationFailure> },
}

impl ValidationOutcome {
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }
}

/// A compiled schema + its failure mode. Transport-agnostic.
#[derive(Clone)]
pub struct CompiledValidator {
    inner: Arc<jsonschema::Validator>,
    failure_mode: FailureMode,
}

impl std::fmt::Debug for CompiledValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledValidator")
            .field("failure_mode", &self.failure_mode)
            .finish_non_exhaustive()
    }
}

impl CompiledValidator {
    /// Build a validator from an already-resolved schema document.
    ///
    /// `failure_mode` is carried through so callers can inspect it without
    /// re-reading config.
    pub fn from_schema(
        schema: &serde_json::Value,
        failure_mode: FailureMode,
    ) -> Result<Self, ValidationConfigError> {
        let validator =
            jsonschema::validator_for(schema).map_err(|e| ValidationConfigError::Compile {
                message: e.to_string(),
            })?;
        Ok(Self {
            inner: Arc::new(validator),
            failure_mode,
        })
    }

    /// Build a validator from a [`ValidationConfig`]. If the config is
    /// file-backed, `resolve_path` is called to turn the raw template into
    /// a concrete filesystem path — callers typically do template
    /// interpolation + `Path::new` there.
    pub fn from_config<F>(
        cfg: &ValidationConfig,
        mut resolve_path: F,
    ) -> Result<Self, ValidationConfigError>
    where
        F: FnMut(&str) -> PathBuf,
    {
        if let Some(schema) = &cfg.inline {
            return Self::from_schema(schema, cfg.on_failure);
        }
        if let Some(path_tmpl) = &cfg.file {
            let path = resolve_path(path_tmpl);
            let schema = load_schema_from_path(&path)?;
            return Self::from_schema(&schema, cfg.on_failure);
        }
        Err(ValidationConfigError::Invalid {
            message: "validation: one of `inline` or `file` is required".into(),
        })
    }

    pub fn failure_mode(&self) -> FailureMode {
        self.failure_mode
    }

    /// Validate an instance. Collects every failure — not just the first.
    pub fn validate(&self, instance: &serde_json::Value) -> ValidationOutcome {
        let mut failures: Vec<ValidationFailure> = Vec::new();
        for err in self.inner.iter_errors(instance) {
            failures.push(ValidationFailure {
                instance_path: err.instance_path().to_string(),
                schema_path: err.schema_path().to_string(),
                keyword: err.kind().keyword().to_string(),
                message: err.to_string(),
            });
        }
        if failures.is_empty() {
            ValidationOutcome::Valid
        } else {
            ValidationOutcome::Invalid { errors: failures }
        }
    }
}

fn load_schema_from_path(path: &Path) -> Result<serde_json::Value, ValidationConfigError> {
    let bytes = std::fs::read(path).map_err(|e| ValidationConfigError::FileRead {
        path: path.to_path_buf(),
        source: e,
    })?;
    serde_json::from_slice(&bytes).map_err(|e| ValidationConfigError::FileParse {
        path: path.to_path_buf(),
        source: e,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn noop_resolve(tmpl: &str) -> PathBuf {
        PathBuf::from(tmpl)
    }

    #[test]
    fn failure_mode_defaults_to_fail() {
        assert_eq!(FailureMode::default(), FailureMode::Fail);
    }

    #[test]
    fn config_requires_source() {
        let v = json!({});
        let err = ValidationConfig::from_json(&v).unwrap_err();
        assert!(err.to_string().contains("required"));
    }

    #[test]
    fn config_rejects_both_sources() {
        let v = json!({
            "inline": { "type": "object" },
            "file": "/tmp/x.json",
        });
        let err = ValidationConfig::from_json(&v).unwrap_err();
        assert!(err.to_string().contains("exactly one"));
    }

    #[test]
    fn config_accepts_inline() {
        let v = json!({ "inline": { "type": "object" } });
        let cfg = ValidationConfig::from_json(&v).unwrap();
        assert!(cfg.inline.is_some());
        assert_eq!(cfg.on_failure, FailureMode::Fail);
    }

    #[test]
    fn config_accepts_file_with_passthrough_mode() {
        let v = json!({ "file": "/tmp/s.json", "on_failure": "passthrough" });
        let cfg = ValidationConfig::from_json(&v).unwrap();
        assert_eq!(cfg.file.as_deref(), Some("/tmp/s.json"));
        assert_eq!(cfg.on_failure, FailureMode::Passthrough);
    }

    #[test]
    fn compile_rejects_bad_schema() {
        // `type` must be a string or array — a number is not valid.
        let bad = json!({ "type": 42 });
        let err = CompiledValidator::from_schema(&bad, FailureMode::Fail).unwrap_err();
        assert!(matches!(err, ValidationConfigError::Compile { .. }));
    }

    #[test]
    fn validate_ok_returns_valid() {
        let schema = json!({ "type": "object", "required": ["name"] });
        let v = CompiledValidator::from_schema(&schema, FailureMode::Fail).unwrap();
        let outcome = v.validate(&json!({ "name": "alice" }));
        assert!(outcome.is_valid());
    }

    #[test]
    fn validate_fail_collects_all_errors() {
        let schema = json!({
            "type": "object",
            "required": ["name", "age"],
            "properties": { "age": { "type": "integer", "minimum": 0 } }
        });
        let v = CompiledValidator::from_schema(&schema, FailureMode::Fail).unwrap();
        let outcome = v.validate(&json!({ "age": -1 }));
        match outcome {
            ValidationOutcome::Valid => panic!("expected invalid"),
            ValidationOutcome::Invalid { errors } => {
                assert!(!errors.is_empty());
                // Should include at least the missing required + minimum failures.
                let keywords: Vec<&str> = errors.iter().map(|e| e.keyword.as_str()).collect();
                assert!(
                    keywords.contains(&"required") || keywords.contains(&"minimum"),
                    "expected required or minimum keyword in {keywords:?}",
                );
            }
        }
    }

    #[test]
    fn validator_from_inline_config() {
        let cfg = ValidationConfig::from_json(&json!({
            "inline": { "type": "string" }
        }))
        .unwrap();
        let v = CompiledValidator::from_config(&cfg, noop_resolve).unwrap();
        assert!(v.validate(&json!("hi")).is_valid());
        assert!(!v.validate(&json!(1)).is_valid());
    }

    #[test]
    fn validator_from_file_config_with_template_interpolation() {
        // Write a schema to disk.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let schema_text = serde_json::to_string(&json!({
            "type": "object",
            "required": ["id"]
        }))
        .unwrap();
        std::fs::write(tmp.path(), schema_text).unwrap();

        // Use a templated path and supply an interpolation closure.
        let cfg = ValidationConfig::from_json(&json!({
            "file": "{schema_path}"
        }))
        .unwrap();
        let tmp_path = tmp.path().to_string_lossy().into_owned();
        let resolver = |tmpl: &str| PathBuf::from(tmpl.replace("{schema_path}", &tmp_path));

        let v = CompiledValidator::from_config(&cfg, resolver).unwrap();
        assert!(v.validate(&json!({ "id": 1 })).is_valid());
        assert!(!v.validate(&json!({})).is_valid());
    }

    #[test]
    fn file_config_missing_file_errors_out() {
        let cfg =
            ValidationConfig::from_json(&json!({ "file": "/nope/does/not/exist.json" })).unwrap();
        let err = CompiledValidator::from_config(&cfg, noop_resolve).unwrap_err();
        assert!(matches!(err, ValidationConfigError::FileRead { .. }));
    }

    #[test]
    fn file_config_bad_json_errors_out() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "{ not json").unwrap();
        let cfg =
            ValidationConfig::from_json(&json!({ "file": tmp.path().to_string_lossy() })).unwrap();
        let err = CompiledValidator::from_config(&cfg, noop_resolve).unwrap_err();
        assert!(matches!(err, ValidationConfigError::FileParse { .. }));
    }
}
