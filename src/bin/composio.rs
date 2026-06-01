//! Minimal host runner for Composio-backed psflow graphs.
//!
//! The stock `psflow` binary wires `NodeRegistry::with_defaults` and installs no
//! `SecretResolver`, so it cannot execute a graph that references an auth
//! strategy. This runner wires `with_defaults_full` plus an env-backed resolver
//! so `%% @graph auth.*.secrets.<role>: <ENV_VAR>` resolves from the process
//! environment. It is the Phase-1 prototype runner for the Composio integration.
//!
//! Usage:
//!   COMPOSIO_API_KEY=sk_... cargo run --bin composio --features runtime -- examples/composio_tool_execute.mmd

use async_trait::async_trait;
use psflow::auth::{SecretError, SecretRequest, SecretResolver, SecretValue};
use psflow::execute::{
    auto_install_auth_registry, ExecutionContext, ExecutionEvent, TopologicalExecutor,
};
use psflow::registry::NodeRegistry;
use psflow::scripting::engine::ScriptEngine;
use psflow::{load_mermaid, Executor, NodeState};
use std::env;
use std::process::ExitCode;
use std::sync::Arc;

/// Resolves a secret's `logical_name` directly from an environment variable.
/// The graph maps a strategy role to an env var name, e.g.
/// `auth.composio.secrets.token: COMPOSIO_API_KEY`, so `logical_name` is the
/// env var to read.
struct EnvResolver;

#[async_trait]
impl SecretResolver for EnvResolver {
    async fn resolve(&self, req: &SecretRequest) -> Result<SecretValue, SecretError> {
        env::var(&req.logical_name)
            .map(|v| SecretValue::new(v.into_bytes()))
            .map_err(|_| SecretError::NotFound {
                strategy: req.strategy_name.clone(),
                logical_name: req.logical_name.clone(),
            })
    }
}

fn main() -> ExitCode {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: composio <graph.mmd>");
        eprintln!("  resolves auth secrets from environment variables (e.g. COMPOSIO_API_KEY)");
        return ExitCode::FAILURE;
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: cannot read '{path}': {e}");
            return ExitCode::FAILURE;
        }
    };

    let graph = match load_mermaid(&content) {
        Ok(g) => g,
        Err(errors) => {
            for e in &errors {
                eprintln!("parse error: {e}");
            }
            return ExitCode::FAILURE;
        }
    };

    let name = graph.metadata().name.as_deref().unwrap_or("(unnamed)");
    eprintln!(
        "loaded graph '{}': {} nodes, {} edges",
        name,
        graph.node_count(),
        graph.edge_count()
    );

    // Build the context, install the env-backed resolver, then build the auth
    // registry from the graph's declared strategies.
    let mut ctx = ExecutionContext::new();
    ctx.set_secret_resolver(Arc::new(EnvResolver));
    let ctx = Arc::new(ctx);
    if let Err(e) = auto_install_auth_registry(&graph, &ctx) {
        eprintln!("auth setup error: {e}");
        return ExitCode::FAILURE;
    }

    // with_defaults_full overrides http/ws with the context-bound, auth-aware
    // handlers that read the resolver and registry from this same ctx.
    let engine = Arc::new(ScriptEngine::with_defaults());
    let handlers = NodeRegistry::with_defaults_full(engine, ctx.clone()).into_handler_registry();

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot create tokio runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = match rt.block_on(TopologicalExecutor::new().execute(&graph, &handlers)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("execution error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut states: Vec<_> = result.node_states.iter().collect();
    states.sort_by_key(|(id, _)| (*id).clone());
    for (id, state) in &states {
        let symbol = match state {
            NodeState::Completed => "+",
            NodeState::Failed => "!",
            NodeState::Cancelled => "~",
            _ => "?",
        };
        eprintln!("  [{symbol}] {id}: {state}");
        if let Some(outputs) = result.node_outputs.get(*id) {
            if !outputs.is_empty() {
                eprintln!("      {outputs:?}");
            }
        }
        if **state == NodeState::Failed {
            for ev in &result.events {
                if let ExecutionEvent::NodeFailed { node_id, error } = ev {
                    if node_id == *id {
                        eprintln!("      error: {error}");
                    }
                }
            }
        }
    }

    let failed: Vec<_> = result
        .node_states
        .iter()
        .filter(|(_, s)| **s == NodeState::Failed)
        .map(|(id, _)| id.as_str())
        .collect();

    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        eprintln!("failed nodes: {}", failed.join(", "));
        ExitCode::FAILURE
    }
}
