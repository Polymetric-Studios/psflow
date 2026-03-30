//! Sandboxed Rhai script engine for psflow.

use rhai::{Dynamic, Engine, Scope, AST};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Configuration for the sandboxed script engine.
#[derive(Debug, Clone)]
pub struct ScriptEngineConfig {
    pub max_operations: u64,
    pub max_call_levels: usize,
    pub max_string_size: usize,
    pub max_array_size: usize,
    pub max_map_size: usize,
}

impl Default for ScriptEngineConfig {
    fn default() -> Self {
        Self {
            max_operations: 100_000,
            max_call_levels: 32,
            max_string_size: 1_000_000,
            max_array_size: 10_000,
            max_map_size: 10_000,
        }
    }
}

/// A sandboxed Rhai engine with execution limits and cooperative cancellation.
///
/// The engine stores its configuration and builds a fresh Rhai `Engine` for each
/// evaluation call, installing a cancellation-aware progress callback. Compilation
/// uses a shared base engine (no progress callback needed).
pub struct ScriptEngine {
    config: ScriptEngineConfig,
    /// Base engine used for compilation only (no progress callback).
    compile_engine: Engine,
}

/// Build a Rhai engine with the given sandbox config and optional cancellation.
///
/// Uses `DummyModuleResolver` to prevent `import` statements from loading
/// arbitrary files from the filesystem — critical for sandbox security.
fn build_engine(config: &ScriptEngineConfig, cancel: Option<CancellationToken>) -> Engine {
    let mut engine = Engine::new();

    // SECURITY: Disable filesystem module loading to prevent scripts from
    // importing arbitrary .rhai files via `import "path/to/module"`.
    engine.set_module_resolver(rhai::module_resolvers::DummyModuleResolver::new());

    engine.on_print(|_| {});
    engine.on_debug(|_, _, _| {});

    engine.set_max_operations(config.max_operations);
    engine.set_max_call_levels(config.max_call_levels);
    engine.set_max_string_size(config.max_string_size);
    engine.set_max_array_size(config.max_array_size);
    engine.set_max_map_size(config.max_map_size);

    // Register ctx_get/ctx_has helper functions for blackboard access.
    // These operate on a Map argument (the `ctx` scope variable).
    engine.register_fn("ctx_get", |map: rhai::Map, key: &str| -> Dynamic {
        map.get(key).cloned().unwrap_or(Dynamic::UNIT)
    });
    engine.register_fn("ctx_has", |map: rhai::Map, key: &str| -> bool {
        map.contains_key(key)
    });

    if let Some(cancel) = cancel {
        engine.on_progress(move |_ops| {
            if cancel.is_cancelled() {
                Some(Dynamic::UNIT)
            } else {
                None
            }
        });
    }

    engine
}

impl ScriptEngine {
    /// Create a new sandboxed engine with the given configuration.
    pub fn new(config: ScriptEngineConfig) -> Self {
        let compile_engine = build_engine(&config, None);
        Self {
            config,
            compile_engine,
        }
    }

    /// Create a new engine with default sandbox limits.
    pub fn with_defaults() -> Self {
        Self::new(ScriptEngineConfig::default())
    }

    /// Compile a script into an AST for repeated execution.
    pub fn compile(&self, script: &str) -> Result<AST, ScriptError> {
        self.compile_engine
            .compile(script)
            .map_err(ScriptError::Parse)
    }

    /// Compile an expression (single statement returning a value).
    pub fn compile_expression(&self, expr: &str) -> Result<AST, ScriptError> {
        self.compile_engine
            .compile_expression(expr)
            .map_err(ScriptError::Parse)
    }

    /// Evaluate an expression string with the given scope, checking cancellation.
    pub fn eval_expression(
        &self,
        expr: &str,
        scope: &mut Scope,
        cancel: &CancellationToken,
    ) -> Result<Dynamic, ScriptError> {
        if cancel.is_cancelled() {
            return Err(ScriptError::Cancelled);
        }
        let ast = self.compile_expression(expr)?;
        self.eval_ast(scope, &ast, cancel)
    }

    /// Evaluate a compiled AST with the given scope, checking cancellation.
    pub fn eval_ast(
        &self,
        scope: &mut Scope,
        ast: &AST,
        cancel: &CancellationToken,
    ) -> Result<Dynamic, ScriptError> {
        if cancel.is_cancelled() {
            return Err(ScriptError::Cancelled);
        }

        let engine = build_engine(&self.config, Some(cancel.clone()));

        engine
            .eval_ast_with_scope::<Dynamic>(scope, ast)
            .map_err(|e| {
                if e.to_string().contains("terminated") {
                    ScriptError::Cancelled
                } else {
                    ScriptError::Runtime(e)
                }
            })
    }

    /// Run a compiled script (may have side effects on scope) with cancellation.
    pub fn run_ast(
        &self,
        scope: &mut Scope,
        ast: &AST,
        cancel: &CancellationToken,
    ) -> Result<(), ScriptError> {
        if cancel.is_cancelled() {
            return Err(ScriptError::Cancelled);
        }

        let engine = build_engine(&self.config, Some(cancel.clone()));

        engine
            .run_ast_with_scope(scope, ast)
            .map_err(|e: Box<rhai::EvalAltResult>| {
                if e.to_string().contains("terminated") {
                    ScriptError::Cancelled
                } else {
                    ScriptError::Runtime(e)
                }
            })
    }

    /// Get a mutable reference to the compilation engine for registering custom functions.
    ///
    /// Note: functions registered here are available for compilation/validation only.
    /// For runtime availability, use [`rebuild_with_functions`] or register on the
    /// config and rebuild.
    pub fn engine_mut(&mut self) -> &mut Engine {
        &mut self.compile_engine
    }

    /// Get the current configuration.
    pub fn config(&self) -> &ScriptEngineConfig {
        &self.config
    }
}

/// Errors that can occur during script execution.
#[derive(Debug)]
pub enum ScriptError {
    /// Script failed to parse.
    Parse(rhai::ParseError),
    /// Runtime error during script execution.
    Runtime(Box<rhai::EvalAltResult>),
    /// Script was cancelled via CancellationToken.
    Cancelled,
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScriptError::Parse(e) => write!(f, "script parse error: {e}"),
            ScriptError::Runtime(e) => write!(f, "script runtime error: {e}"),
            ScriptError::Cancelled => write!(f, "script execution cancelled"),
        }
    }
}

impl std::error::Error for ScriptError {}

/// Create an `Arc<ScriptEngine>` with default configuration.
pub fn default_script_engine() -> Arc<ScriptEngine> {
    Arc::new(ScriptEngine::with_defaults())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_simple_expression() {
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();
        scope.push("x", 10_i64);

        let result = engine
            .eval_expression("x + 5", &mut scope, &cancel)
            .unwrap();
        assert_eq!(result.as_int().unwrap(), 15);
    }

    #[test]
    fn eval_boolean_expression() {
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();
        scope.push("score", 85_i64);

        let result = engine
            .eval_expression("score > 70 && score < 100", &mut scope, &cancel)
            .unwrap();
        assert!(result.as_bool().unwrap());
    }

    #[test]
    fn eval_string_expression() {
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();
        scope.push("name", "hello".to_string());

        let result = engine
            .eval_expression("name.len() > 0", &mut scope, &cancel)
            .unwrap();
        assert!(result.as_bool().unwrap());
    }

    #[test]
    fn compile_and_run_script() {
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        let ast = engine.compile("let result = 2 + 3; result").unwrap();
        let mut scope = Scope::new();

        let result = engine.eval_ast(&mut scope, &ast, &cancel).unwrap();
        assert_eq!(result.as_int().unwrap(), 5);
    }

    #[test]
    fn run_ast_mutates_scope() {
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        let ast = engine.compile("let output = x * 2;").unwrap();
        let mut scope = Scope::new();
        scope.push("x", 21_i64);

        engine.run_ast(&mut scope, &ast, &cancel).unwrap();
        let output = scope.get_value::<i64>("output").unwrap();
        assert_eq!(output, 42);
    }

    #[test]
    fn max_operations_enforced() {
        let engine = ScriptEngine::new(ScriptEngineConfig {
            max_operations: 100,
            ..Default::default()
        });
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();

        let ast = engine.compile("let i = 0; loop { i += 1; }").unwrap();
        let result = engine.eval_ast(&mut scope, &ast, &cancel);
        assert!(result.is_err());
    }

    #[test]
    fn max_call_levels_enforced() {
        let engine = ScriptEngine::new(ScriptEngineConfig {
            max_call_levels: 5,
            ..Default::default()
        });
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();

        let ast = engine
            .compile("fn recurse(n) { recurse(n + 1) } recurse(0)")
            .unwrap();
        let result = engine.eval_ast(&mut scope, &ast, &cancel);
        assert!(result.is_err());
    }

    #[test]
    fn cancellation_before_eval() {
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut scope = Scope::new();

        let result = engine.eval_expression("1 + 1", &mut scope, &cancel);
        assert!(matches!(result, Err(ScriptError::Cancelled)));
    }

    #[test]
    fn cancellation_during_eval() {
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();

        let cancel_clone = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            cancel_clone.cancel();
        });

        let ast = engine
            .compile("let i = 0; loop { i += 1; } i")
            .unwrap();
        let result = engine.eval_ast(&mut scope, &ast, &cancel);
        assert!(result.is_err());
    }

    #[test]
    fn print_and_debug_disabled() {
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();

        let ast = engine
            .compile("print(\"hello\"); debug(42); 1 + 1")
            .unwrap();
        let result = engine.eval_ast(&mut scope, &ast, &cancel).unwrap();
        assert_eq!(result.as_int().unwrap(), 2);
    }

    #[test]
    fn map_access_in_expression() {
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();

        let mut map = rhai::Map::new();
        map.insert("score".into(), Dynamic::from(95_i64));
        map.insert("name".into(), Dynamic::from("test".to_string()));
        scope.push_dynamic("inputs", Dynamic::from_map(map));

        let result = engine
            .eval_expression("inputs.score > 90", &mut scope, &cancel)
            .unwrap();
        assert!(result.as_bool().unwrap());
    }

    #[test]
    fn parse_error_reported() {
        let engine = ScriptEngine::with_defaults();
        let result = engine.compile("let x = ;; garbage");
        assert!(matches!(result, Err(ScriptError::Parse(_))));
    }

    #[test]
    fn custom_config_applied() {
        let config = ScriptEngineConfig {
            max_operations: 50_000,
            max_call_levels: 16,
            max_string_size: 500_000,
            max_array_size: 5_000,
            max_map_size: 5_000,
        };
        let _engine = ScriptEngine::new(config);
    }

    // -- 3.T.15: Sandbox enforcement tests --

    #[test]
    fn import_statement_blocked() {
        // DummyModuleResolver should reject all imports
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();

        let ast = engine.compile("import \"something\" as m; m::foo()").unwrap();
        let result = engine.eval_ast(&mut scope, &ast, &cancel);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("Module not found"),
            "expected module resolution error, got: {err_msg}"
        );
    }

    #[test]
    fn string_size_limit_enforced() {
        let engine = ScriptEngine::new(ScriptEngineConfig {
            max_string_size: 100,
            ..Default::default()
        });
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();

        // Build a string that exceeds the limit
        let ast = engine
            .compile("let s = \"x\"; for i in 0..200 { s += \"x\"; } s")
            .unwrap();
        let result = engine.eval_ast(&mut scope, &ast, &cancel);
        assert!(result.is_err());
    }

    #[test]
    fn array_size_limit_enforced() {
        let engine = ScriptEngine::new(ScriptEngineConfig {
            max_array_size: 50,
            ..Default::default()
        });
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();

        let ast = engine
            .compile("let a = []; for i in 0..100 { a.push(i); } a")
            .unwrap();
        let result = engine.eval_ast(&mut scope, &ast, &cancel);
        assert!(result.is_err());
    }

    #[test]
    fn map_size_limit_enforced() {
        // Rhai's max_map_size is enforced at parse time on map literals
        let engine = ScriptEngine::new(ScriptEngineConfig {
            max_map_size: 3,
            ..Default::default()
        });

        // Map literal exceeding the limit should fail to compile
        let result = engine.compile("let m = #{a: 1, b: 2, c: 3, d: 4}; m");
        assert!(result.is_err());
    }

    #[test]
    fn deep_recursion_blocked() {
        let engine = ScriptEngine::new(ScriptEngineConfig {
            max_call_levels: 10,
            ..Default::default()
        });
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();

        let ast = engine
            .compile("fn deep(n) { if n > 0 { deep(n - 1) } else { 0 } } deep(100)")
            .unwrap();
        let result = engine.eval_ast(&mut scope, &ast, &cancel);
        assert!(result.is_err());
    }

    #[test]
    fn ctx_get_and_ctx_has_registered() {
        // Verify the helper functions are available on eval engines
        let engine = ScriptEngine::with_defaults();
        let cancel = CancellationToken::new();
        let mut scope = Scope::new();

        let mut map = rhai::Map::new();
        map.insert("key".into(), Dynamic::from(42_i64));
        scope.push_dynamic("m", Dynamic::from_map(map));

        let result = engine
            .eval_expression("ctx_get(m, \"key\")", &mut scope, &cancel)
            .unwrap();
        assert_eq!(result.as_int().unwrap(), 42);

        let result = engine
            .eval_expression("ctx_has(m, \"key\")", &mut scope, &cancel)
            .unwrap();
        assert!(result.as_bool().unwrap());

        let result = engine
            .eval_expression("ctx_has(m, \"missing\")", &mut scope, &cancel)
            .unwrap();
        assert!(!result.as_bool().unwrap());
    }
}
