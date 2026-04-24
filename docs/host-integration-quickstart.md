# Host Integration Quickstart

Audience: a Rust engineer embedding psflow into a consumer project for the first time.

---

## 1. What you'll build

A runnable psflow graph that declares a bearer-token auth strategy at graph scope, makes an authenticated GET against an external API, and returns the response body and status code as typed outputs. By the end you will have a working `ExecutionContext`, a `NodeRegistry` wired with auth, and an async call to `TopologicalExecutor` that hands you the result.

---

## 2. Prerequisites

Add psflow and the tokio runtime to your `Cargo.toml`:

```toml
[dependencies]
psflow = { path = "../psflow", features = ["runtime"] }
tokio = { version = "1", features = ["full"] }
anyhow = "1"
async-trait = "0.1"
```

psflow's `runtime` feature pulls in `reqwest`, `rhai`, `tokio-tungstenite`, and everything else the network handlers need. You do not need to add those crates directly.

---

## 3. Implement a `SecretResolver`

psflow declares the trait; your project implements it. The resolver is the only place your project's secret storage (env vars, a vault, a test fixture) touches psflow.

```rust
use psflow::auth::{SecretRequest, SecretResolver, SecretValue};
use psflow::auth::SecretError;
use async_trait::async_trait;
use std::env;

struct EnvResolver;

#[async_trait]
impl SecretResolver for EnvResolver {
    async fn resolve(&self, req: &SecretRequest) -> Result<SecretValue, SecretError> {
        // Use logical_name as the env var name. Ignore strategy_name here —
        // a namespaced vault path would use both fields.
        env::var(&req.logical_name)
            .map(|v| SecretValue::new(v.into_bytes()))
            .map_err(|_| SecretError::NotFound {
                strategy: req.strategy_name.clone(),
                logical_name: req.logical_name.clone(),
            })
    }
}
```

Set `API_TOKEN` in your environment before running:

```bash
export API_TOKEN=my-secret-token
```

---

## 4. Author the graph

Save this as `my_graph.mmd`. It declares one bearer strategy named `api` and one HTTP node that references it.

```mermaid
graph TD
    Fetch[Fetch Widget]

    %% @graph name: "widget-fetch"

    %% Declare auth strategy at graph scope.
    %% The resolver will be called with logical_name="API_TOKEN".
    %% @graph auth.api.type: bearer
    %% @graph auth.api.secrets.token: API_TOKEN

    %% HTTP node opts in by referencing the strategy name.
    %% @Fetch handler: http
    %% @Fetch config.url: "https://api.example.com/widgets/1"
    %% @Fetch config.method: GET
    %% @Fetch config.auth: api
```

Two annotation forms are used here:

- `%% @graph auth.api.type: bearer` — sets a graph-level field using the `auth.<name>.<field>` dot-path.
- `%% @Fetch config.auth: api` — sets a node config field using `config.<key>`.

---

## 5. Wire it up and run

```rust
use psflow::{
    load_mermaid,
    execute::{auto_install_auth_registry, ExecutionContext, TopologicalExecutor},
    registry::NodeRegistry,
    scripting::engine::ScriptEngine,
    Executor,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load the graph from the .mmd file.
    let mmd = std::fs::read_to_string("my_graph.mmd")?;
    let graph = load_mermaid(&mmd)?;

    // Build the execution context and install your secret resolver.
    let mut ctx = ExecutionContext::new();
    ctx.set_secret_resolver(Arc::new(EnvResolver));
    let ctx = Arc::new(ctx);

    // auto_install_auth_registry reads graph.metadata().auth, builds an
    // AuthStrategyRegistry from the built-in factories, and installs it on
    // the context. Call this after the resolver is set, before execution.
    auto_install_auth_registry(&graph, &ctx)?;

    // Build a handler registry. with_defaults_full binds context-dependent
    // handlers (accumulator, break, select) and overrides the stateless
    // http/ws handlers with context-bound ones so config.auth resolves.
    let engine = Arc::new(ScriptEngine::with_defaults());
    let node_reg = NodeRegistry::with_defaults_full(engine, ctx.clone());
    let handlers = node_reg.into_handler_registry();

    // Execute.
    let result = TopologicalExecutor::new()
        .execute(&graph, &handlers)
        .await?;

    // result.node_outputs is a map from node ID to Outputs.
    let fetch_out = result.node_outputs.get("Fetch").expect("Fetch node ran");
    println!("status: {:?}", fetch_out.get("status"));
    println!("body:   {:?}", fetch_out.get("body"));

    Ok(())
}
```

Key points:

- `auto_install_auth_registry` is a one-liner that covers the common case. It only runs if `graph.metadata().auth` is non-empty and the context has no registry already installed. If you need to register custom strategy types, build and install the registry manually before calling `auto_install_auth_registry` — the function skips installation if a registry is already present.
- `NodeRegistry::with_defaults_full` registers the context-bound `http` and `ws` handlers. The stateless variants registered by `with_defaults` cannot resolve `config.auth` and will fail at execution time if a node references a strategy.
- `TopologicalExecutor` runs nodes in dependency order, parallelising independent waves. It calls `auto_install_auth_registry` internally as well; the explicit call above is a belt-and-suspenders guard that surfaces load-time errors before execution starts.

---

## 6. Verify

A successful run produces an `Outputs` map on the `Fetch` node with at least:

- `status` — `Value::I64(200)` (or whatever the server returned)
- `body` — `Value::String("...")` containing the response body
- `headers` — `Value::Map(...)` of response headers

If the bearer token was incorrect the server returns a non-2xx status. The node still succeeds (psflow passes status and body through by default); set `config.fail_on_non_2xx: true` on the node to turn non-2xx into a node failure instead.

---

## 7. Testing locally

psflow's HTTP handler blocks loopback and private-range IPs by default. If you want to verify the quickstart against a local mock server (e.g. `http://localhost:8080`), add `%% @Fetch config.allow_private: true` to the graph. Without it, requests to `127.0.0.1` or `192.168.x.x` fail at the handler level before a connection is attempted.

---

## 8. Next steps

- For the full list of config keys per handler — HTTP body shapes, retry, redirect, validation, body_sink, WS, poll_until — see [docs/mermaid-annotation-reference.md](./mermaid-annotation-reference.md).
- For other auth strategies (`static_header`, `hmac`, `cookie_jar`) and their params, see the same reference doc, section 2.
- To register a custom strategy type (e.g. an OAuth2 strategy your project provides), build an `AuthStrategyRegistry`, call `registry.register_factory("oauth2", Arc::new(your_factory))`, call `registry.build_from_decls(graph.metadata().auth)`, then call `ctx.install_auth_registry(registry)` before the executor runs. Do this before `auto_install_auth_registry` would otherwise fire.
