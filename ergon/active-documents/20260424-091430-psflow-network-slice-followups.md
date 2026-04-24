20260424-091430-psflow-network-slice-followups.md

# psflow network-slice follow-ups

## 1. Context

The network slice (commit `45bdfe8a`) shipped auth, HTTP gap-fill, validation, body_sink, WebSocket, and poll_until. This doc captures every open thread surfaced during that work, plus items flagged as out-of-scope at the time. Nothing here is urgent; each is a candidate, not a commitment.

## 2. Auth layer

- [ ] **Auto-install `AuthStrategyRegistry` from `GraphMetadata.auth`** — executors currently don't construct the registry from declared strategies; embedders must call `ctx.install_auth_registry(...)`. Add an `Executor::with_graph` (or equivalent) hook so registry installation is automatic when a graph declares auth.
- [ ] **HMAC header coverage audit** — the canonical string signs only headers set by the handler plus the key-id header because `reqwest::RequestBuilder` doesn't expose previously-set headers without `try_clone+build`. Decide whether to adopt the `try_clone+build` inspection path before any vendor-specific strategy (SigV4, Stripe, etc.) is added. Flagged in `src/auth/strategies/hmac.rs`.
- [ ] **`observe_response` semantics on retries** — currently fires on every attempt (correct for cookie jar rotating session cookies). Consider whether strategies that treat `observe_response` as "final" need an opt-in `observe_each_attempt: bool` or similar, or whether "final-only" is a separate hook. No known consumer yet.
- [ ] **Composite strategy built-in** — design doc §8 locked "one strategy per node". If layered auth (e.g. bearer + HMAC) ever lands, ship it as a `Composite` built-in that wraps an ordered list, rather than reopening node-level chaining.

## 3. HTTP handler polish

- [x] ~~**Prune redirect config synonyms** — `redirect` accepts both `"limited": n` and `"max": n`. Pick one before consumer use so the annotation surface doesn't fork.~~ (done: kept `max`, removed `limited`)
- [ ] **Exponential backoff + jitter on `poll_until`** — not in minimum surface; extension candidate if/when a consumer hits a rate-limited polling target.

## 4. Validation module

- [ ] **Validator cache eviction** — `HttpHandler` caches compiled validators by raw config JSON keyed on the handler lifetime. Acceptable now; revisit if a graph rotates many inline schemas per run.
- [ ] **Schema-file path interpolation** — currently uses `{key}`-style interpolation over `inputs` only. Swap to full `PromptTemplate` resolution (blackboard / ctx) if consumers need it. Matches HTTP URL/header templating today.

## 5. WebSocket handler

- [ ] **Surface `TerminationReason::ValidationError`** — defined but unreachable under current semantics (fail mode returns `Err`; passthrough mode continues). Add an explicit surfacing path if embedders want to branch on validation-driven termination without intercepting the `Failed` variant.
- [ ] **`close_on_terminate` flush guarantee** — best-effort under `tokio::select!` races on timeout/cancellation. Consider an explicit drain-and-flush path if a consumer hits a server that strictly requires a graceful close.
- [x] ~~**Thread `WS_HANDLER_NAME` const through call sites** — currently the literal `"ws"` is used in `src/registry.rs` and `AuthStrategyRegistry::validate_graph`. Small polish.~~ (done)
- [ ] **Subprotocol support** — skipped in workstream 5 unless trivial. If a consumer needs one, `tokio-tungstenite` config takes it; extend the config surface then.

## 6. poll_until and subgraph semantics

- [ ] **Child-failure propagation in `SubgraphInvocationHandler`** — currently converts in-graph child failures to empty output maps. `poll_until` inherits this. Consider an opt-in `propagate_child_failures: bool` on `SubgraphInvocationHandler` so `poll_until` predicates can detect attempt-level failures distinctly from empty success maps.

## 7. Cross-cutting: load-time validation

- [ ] **`validate_graph` hook across handlers** — several handlers do load-time checks at handler-execute entry because there's no earlier seam with `node.config` access. Introduce a proper graph-load validation pass so config errors fail before a long run starts. Handlers that would benefit: `HttpHandler` (validation/body_sink incompat), `WebSocketHandler` (auth-strategy WS support), `PollUntilHandler` (subgraph existence, predicate compile), `AuthStrategyRegistry` (shape validation).

## 8. Documentation / examples

- [ ] **Host-integration quickstart** — example showing an embedder wiring `SecretResolver` + `AuthStrategyRegistry` into `ExecutionContext`, plus a minimal graph using bearer auth over HTTP.
- [ ] **Mermaid annotation cheat-sheet for new config surfaces** — auth declarations, retry/backoff config, multipart, body_sink, validation, WebSocket termination, poll_until. Existing `config.<key>=<value>` passthrough handles the parsing, but a concise reference reduces trial-and-error for graph authors.

## 9. Out of scope (recorded, not tasks)

- Secret storage, rotation, lifecycle — host concern.
- Capture-to-graph synthesis — consumer tooling.
- MCP exposure of graphs — consumer tooling.
- Service-specific schemas and fixtures — consumer tooling.
- WS reconnection / resume — author retry or subgraph around a WS node.
- Permessage-deflate / compression extensions — not in the proposal.
- Resumable HTTP downloads, content-length honoring, progress callbacks — explicit YAGNI on `body_sink`.
- Node-level HTTP retry (covered by HTTP-scoped retry in workstream 6).
