20260601-091344-composio-psflow-integration-design.md

# Composio ↔ psflow integration design

## 1. Context

Composio is a managed connector/identity layer for AI agents: a large catalog of authenticated SaaS toolkits (GitHub, Gmail, Slack, Notion, Linear, …), managed OAuth/credential storage scoped per `user_id`, direct tool execution over REST, and signed event webhooks (triggers).

psflow already exposes the exact extension sockets Composio fills — a handler registry (`src/registry.rs`), a pluggable auth layer (`AuthStrategy` + `SecretResolver`, `src/auth/`), `Trigger` handler nodes feeding the event-driven executor, and an `AiAdapter` trait for LLM tool-use — but has no managed connector catalog, no OAuth/credential vault, and no per-user connected-account model. Composio supplies a matching plug for each socket.

Composio's execution model — *execute a tool slug with arguments for a user* — is a one-to-one match for a psflow deterministic handler call. This makes the integration additive against existing trait boundaries; the executor core is untouched.

Key realization from the docs sweep: **every Composio capability (direct execution, dynamic tool search, batch fan-out, sandboxed compute) is reachable through one REST execution shape.** The integration is therefore one generic handler plus an auth strategy plus a trigger entry — not a per-capability build.

## 2. Goals and non-goals

### 2.1 Goals

- A single generic handler that exposes any Composio toolkit (and the meta-tools) as a psflow step.
- A Composio-backed auth path so psflow never stores or refreshes external tokens.
- A trigger entry that lets verified Composio events start psflow workflows.
- Optional: Composio tool schemas fed to `llm_call` for agentic tool selection.
- Central, reusable rate-limit / error / observability handling so fan-out graphs are safe.

### 2.2 Decisions locked (20260601)

- **Single-user / personal scope.** This integration is for one operator on their own machine with their own connected accounts. This cuts Phase 2 (managed multi-`user_id` auth), per-account aliasing, org-quota-DoS concerns, and the production "not for runtime" caveats — none apply at one user.
- **Auth via `composio login`** (machine-level), not an api key in the graph. The CLI's logged-in context is the auth; graphs carry no key and no `user_id`.
- **Canonical handler: the native `composio` handler** (`src/handlers/composio.rs`) — wraps the `composio` CLI and returns the parsed envelope as structured outputs. Stateless default → runs on the stock `psflow` binary. Implemented and verified live (§5).
- **Advanced capabilities: only batch fan-out is in scope** (§10.1). Sandboxed shell/workbench, MCP-as-output, and dynamic tool search are parked.
- **Superseded earlier decisions:** the `http`+`x-api-key` path (`examples/composio_tool_execute.mmd` + `src/bin/composio`) and Phases 3–4 remain in the doc as the *multi-user/production* route, retained for reference but not the active path.

### 2.3 Non-goals

- Hosting psflow workflows *as* Composio tools. Composio custom tools are SDK-in-process only (no server-side registration API); the clean direction is psflow → Composio. Recorded in §13.
- Replacing psflow's executor loop with Composio's agent loop or MCP client. psflow keeps step-by-step control; MCP-mode is an *output* surface only (§10).
- Managing Composio auth-config provisioning (`ac_…`) inside psflow. That is a one-time dashboard/SDK setup artifact referenced by id.

## 3. Architecture: socket → plug mapping

- **Handler registry** → generic `composio` handler executing tool slugs over REST. (§5)
- **`SecretResolver` / `AuthStrategy`** → resolves a `user_id` (+ optional connected-account id/alias) instead of a token; Composio injects credentials server-side. (§6)
- **`Trigger` node + event-driven executor** → receives Svix-verified Composio webhook payloads as the graph entry. (§7)
- **`AiAdapter` tool list** → populated from Composio tool schemas for oracle nodes. (§8)

psflow is a **non-agentic provider** in Composio's vocabulary: it transforms schemas and drives discrete execution itself; it does not hand off the run loop. The executor remains the agency.

## 4. The execution contract

The whole of Phase 1 rests on one synchronous call. Verified facts the handler depends on:

- Endpoint: `POST /api/v3.1/tools/execute/{tool_slug}` (`tool_slug` a required path param). Auth header `x-api-key` (project) or `x-user-api-key`. Note the base path is `v3.1`, not `v3`.
- Request body (all fields optional in the schema): `arguments` (object) **or** `text` (natural-language alternative — the two are mutually exclusive); `user_id`; `connected_account_id` (auto-resolved from `user_id` when omitted); `version`; `custom_auth_params`; `custom_connection_data`; `entity_id`; `allow_tracing`.
- Response envelope: `successful` (boolean — this spelling, not `success`), `data` (object), `error` (string|null), `log_id` (string), `session_info` (object|null).
- Behaviour is request/response — no polling, no webhook for execution itself.

### 4.1 Hard facts that shape the design

- **Pin `version` explicitly.** `version` is **per-toolkit** (keys are toolkit slugs; format `YYYYMMDD_NN`), not per-tool. Sources conflict on the default — the REST endpoint schema says it defaults to `latest`, while the versioning guide implies an unspecified call resolves to a base version exposing fewer fields. Either way the handler pins a concrete `YYYYMMDD_NN`, since downstream steps consume typed `data` and `latest` risks silent shape drift. (The SDK also exposes `dangerously_skip_version_check` — do not use it.)
- **Rate limit is org-global on a rolling ~10-minute window.** A fan-out graph can exhaust the whole org quota. Backoff must be central, not per-node.
- **Auth header is a single static key.** The interim `http`-handler path needs only one header — no OAuth wiring — making a no-new-code prototype viable.
- **Proxy execution exists** for un-wrapped APIs: reuse a stored connected account, never set `Authorization`, same-domain constraint, no multipart.

## 5. Phase 1 — generic `composio` handler

Goal: any Composio tool becomes a psflow step via `handler: composio` + a tool slug.

Config surface (annotations): toolkit/tool slug, `user_id` (templated), `arguments` (templated object), pinned `version`, optional `connected_account_id`, api-key reference resolved from the secret layer.

- [x] **Handler scope decided** — one generic `composio` handler keyed by slug (not a family); the slug selects direct-execute, meta-tools, and proxy modes.
- [x] **Interim no-build path (first step)** — done and verified end-to-end. `examples/composio_tool_execute.mmd` calls `POST /api/v3.1/tools/execute/{slug}` through the existing `http` handler, with the api key injected as `x-api-key` via the `bearer` strategy (empty scheme) from secret `COMPOSIO_API_KEY`. A dummy-key run reached Composio and returned the expected 401 envelope (message/status/request_id/suggested_fix), confirming endpoint, auth header, and body shape. A valid key + linked connected account is the only remaining input for a green run.
- [x] **Prototype runner** — `src/bin/composio` (`required-features = ["runtime"]`) wires `with_defaults_full` + an env-backed `SecretResolver` + `auto_install_auth_registry`, and surfaces per-node failure reasons. Needed because the stock `psflow` CLI uses `with_defaults` and installs no resolver, so it cannot execute an auth'd graph.
- [x] **CLI variant — verified live (no key)** — proved the CLI path through the stateless `shell` handler against a real Google Sheets connection (`successful: true`, `total_found: 10`). Superseded by the native handler below for everyday use; kept in git history.
- [x] **Native `composio` handler — implemented and verified live** — `src/handlers/composio.rs`, registered in `with_defaults`. Wraps `composio execute <slug> -d <json>`: config is `tool` + an `arguments` object (string leaves template-resolved, e.g. `{inputs.id}`, no brace collisions), plus `binary`/`timeout_ms`/`dry_run`/`allow_unsuccessful`. Parses the CLI's stdout JSON (banner goes to stderr) into structured, typed outputs: `successful` (Bool), `data` (Map), `error` (String|Null), `log_id` (String). Inherits the CLI's schema validation + connection checks. `schema()` implemented (drift guard passes); 3 unit tests + full lib suite green. Canonical example: `examples/composio_cli_execute.mmd` → live run returned `data.total_found: 10`, navigable typed payload (`data.value.spreadsheets[0].value.name.value`). This is the everyday path for single-user use.

### 5.1 Findings from the prototype (psflow gotchas, recorded)

- The stock `psflow` CLI cannot run auth'd graphs (no `SecretResolver`); auth'd graphs need a host like `src/bin/composio`.
- One Mermaid arrow binds exactly one output→input pair (loader best-effort resolution). Multi-input nodes need one edge per input; the prototype keeps each node single-input and hardcodes the slug in the URL.
- Multiline annotation blocks use `>>>` / `%% <<<`, not `|`. The annotation reference doc was stale and has been corrected.
- `parse_json` is not registered for the `rhai` handler; in-script JSON parsing is unavailable. The prototype prints the raw response envelope instead.
- [ ] **Implement the handler** in the handlers module: build request from templated config, POST, branch on the `successful` flag, map `data` to node outputs, surface `error` as a node failure.
- [ ] **Pin `version`** as a required-or-strongly-defaulted config key; reject `latest` by policy for typed downstream steps.
- [ ] **Capture `log_id`** into node output / run record for forensics.
- [ ] **Register** the handler in the full-defaults registry path and add it to graph validation so unknown-handler graphs still fail fast.
- [ ] **Load-time validation** via the handler's `validate_node` seam: required keys present, version well-formed, arguments shape sane.
- [ ] **Add a second `proxy` mode** for un-wrapped APIs (same-domain endpoint, method, parameters, no `Authorization`).
- [ ] **Tests** — mock-transport unit tests for success, `successful:false`, and transport error; one integration test behind an env-gated api key.

## 6. Phase 2 — managed auth / connected accounts

Goal: psflow makes authenticated calls to user-connected services without holding tokens.

Model: a Composio auth config (`ac_…`) is a one-time per-toolkit blueprint. A connected account (`ca_…`) is created when a user authenticates; statuses run `INITIATED → ACTIVE → INACTIVE/EXPIRED`; Composio auto-refreshes OAuth before expiry. Credentials resolve from `user_id` (+ optional account id/alias) at execute time.

- [ ] **`SecretResolver` Composio variant** — resolves a request to a `user_id` (+ optional `ca_…`/alias) rather than a token. psflow never receives a credential.
- [ ] **`AuthStrategy::Composio`** — pre-run gate that checks connected-account status and surfaces `EXPIRED` as a re-auth-needed error distinct from a tool failure.
- [ ] **Account onboarding node** — a human-in-the-loop step that initiates the connect flow, returns the redirect link, and parks the run until the callback resolves or times out.
- [ ] **Multi-account support** — carry an explicit `ca_…` or alias as node config where a workflow targets a specific account; do not rely on "most recently connected".
- [ ] **Connection-expiry handling** — route the connected-account-expired event to a re-auth notification path so long-lived workflows don't fail mid-run.
- [ ] **Managed vs custom decision** — document that managed OAuth apps impose a ~15-minute polling floor and a shared rate-limit quota; production with tighter triggers or branded consent needs a custom auth config. Record the chosen mode per toolkit.

## 7. Phase 3 — triggers / event entry (deferred; designed, not in current scope)

Goal: a verified Composio event starts a psflow workflow.

Delivery is a webhook (production) or SDK websocket subscribe (dev). The payload carries the trigger slug, trigger id, connected-account id, auth-config id, and `user_id` in metadata, plus an event-specific `data` block. The same channel also delivers connected-account-expired and trigger-disabled events.

- [ ] **Svix webhook verification middleware** — read the three webhook headers, recompute HMAC-SHA256 over `id.timestamp.rawbody`, base64, constant-time compare, enforce ~300s skew. Verify on raw bytes before JSON parse. A Svix-compatible verifier can be reused.
- [ ] **`composio_trigger` node** — declares toolkit + trigger slug + `user_id`; on event, seeds the event-driven executor with `metadata.user_id`, connected-account id, and `data` as run context.
- [ ] **Workflow dispatch** — map `metadata.trigger_slug` to the target workflow.
- [ ] **Lifecycle events** — handle trigger-disabled (mark the node dead) and connected-account-expired (route to re-auth from §6).
- [ ] **Trigger provisioning** — create/upsert a trigger instance via `POST /api/v3.1/trigger_instances/{slug}/upsert` (body: `connected_account_id?`, `trigger_config?`, `toolkit_versions?`), SDK `triggers.create(slug, user_id, connected_account_id, trigger_config)`. Returns `trigger_id` (no documented `ti_` prefix). Wire a provisioning step or document a manual dashboard prerequisite.

## 8. Phase 4 — LLM tool-use (deferred; designed, not in current scope)

Goal: oracle/`llm_call` nodes can select Composio tools dynamically. Build last; depends on Phase 1 for execution.

- [ ] **Schema feed** — fetch tool schemas (via the schema meta-tool) and transform them into the `AiAdapter` tool-list format.
- [ ] **Selection round-trip** — route a model's tool selection back through the Phase 1 handler for execution; append results to conversation history.
- [ ] **Late binding (optional)** — allow a node to resolve a slug from natural-language intent via the search meta-tool instead of a hardcoded slug.

## 9. Cross-cutting: rate limits, errors, observability

Build once in the handler; every mode inherits it.

- [ ] **Central rate-limit backoff** — read remaining-quota and retry-after headers; honour the org-global rolling window; retry only on rate-limit and 5xx. A fan-out graph must not self-exhaust the quota.
- [ ] **Error taxonomy** — parse the structured error envelope (message, status, request id, suggested fix). Branch: auth-refresh-required and 401 → re-auth (not retried); 5xx and rate-limit → retried; 4xx config errors → fail fast with the suggested fix surfaced.
- [ ] **Request-id capture** — stamp the request id from every response into the psflow run record.
- [ ] **Log forensics** — wire `log_id` so a failed step is one logs-API lookup from full request/response reconstruction.

## 10. Meta-tools and advanced capabilities

All reachable through the same handler by slug. Only batch fan-out is in current scope; the rest are parked (recorded for later).

### 10.1 In scope

- [ ] **Batch / fan-out** — a single node executes N tool calls and returns ordered results (by `index`), mapping directly onto the render fan-out + accumulator pattern. Evaluate offloading large payloads to remote storage to keep them out of the transport. Concurrency cap is undocumented (§12) — verify empirically before relying on wide fan-out.

### 10.2 Parked (not in current scope)

- Dynamic tool search — late-bound slug resolution from intent.
- Sandboxed shell / workbench — off-host shell and Python; hard ~180s per-call ceiling and imminent billing.
- MCP as an output surface — a curated, allow-listed Composio MCP server for the agents psflow orchestrates, engine nodes staying on REST. Boundary if revived: engine → REST; orchestrated agents → MCP.

## 11. Reusable graph templates

Ship as starter `.mmd` files; they are the two canonical shapes the cookbooks reduce to.

- [ ] **Event-trigger → enrich → llm → act** — trigger, extract, LLM analyze, then a Composio action (e.g. label/apply). Covers the inbox-labeler and PR-review shapes.
- [ ] **Scheduled fan-out digest** — scheduled trigger, parallel Composio fetch nodes, one LLM digest, one Composio post. Covers the background-agent shape.
- [ ] **Selling-point note** — these cookbooks bound their agent loop with a step cap; psflow replaces that loop with explicit nodes, giving deterministic control. Capture this as positioning.

## 12. Open questions / unknowns

Resolved 20260601 against the live API reference (rendered OpenAPI endpoint pages; the `.md` reference stubs carry no per-endpoint schema):

- [x] **Execute request/response fields** — confirmed. Endpoint `POST /api/v3.1/tools/execute/{tool_slug}`, header `x-api-key`. Body fields and the `successful`/`data`/`error`/`log_id`/`session_info` envelope recorded in §4; `arguments` and `text` are mutually exclusive.
- [x] **Create-trigger call** — confirmed. `POST /api/v3.1/trigger_instances/{slug}/upsert` / SDK `triggers.create(slug, user_id, connected_account_id, trigger_config)`; returns `trigger_id` with no documented `ti_` prefix. Recorded in §7.
- [x] **Version granularity** — confirmed **per-toolkit** (toolkit-slug keys, format `YYYYMMDD_NN`). Recorded in §4.1.
- [ ] **Batch concurrency cap** — still **undocumented**. The batch meta-tool states no array-size or parallelism limit. Verify empirically before relying on wide fan-out (§10.1).
- [ ] **Version default** — source conflict: REST endpoint schema says `version` defaults to `latest`; the versioning guide implies a base version when unspecified. Moot for our design (we pin explicitly), but confirm if any path ever omits `version`.

## 13. Personal infrastructure (single-user)

Foundational pieces around the handler that turn one-off graphs into a standing personal automation system. Implemented in `src/bin/psflow_run.rs` (the `psflow-run` binary).

### 13.1 The runner — implemented and verified

`psflow-run <graph> [--input k=v]...` provides what the stock binary lacks:

- [x] **Named graphs** — resolves `<name>` to `<graphs-dir>/<name>.mmd` (`PSFLOW_GRAPHS_DIR`, default `./graphs`). Convenience recipe: `just graph <name> --input k=v`.
- [x] **Runtime inputs** — `--input k=v` (value parses as JSON, else string) reaches handler templates as `{ctx.key}`. Mechanism: a `RuntimeInputResolver` (custom `TemplateResolver`) merges inputs into each handler's blackboard, because `collect_inputs` only threads upstream node outputs and the stateless `composio`/`shell` handlers render against a fresh blackboard. Verified by A/B: `query=INV` → 5 matches, `query=ZZZ…` → 0.
- [x] **LLM adapter wired** — registers `llm_call` with the keyless `ClaudeCliAdapter`, so Composio-tool → LLM graphs run here (the stock binary omits `llm_call`). Verified live.
- [x] **Run history** — each run writes `<runs-dir>/<ts>-<graph>.json` with status, per-node states, the execution trace, and every Composio `log_id` (for one-lookup forensics). `runs/` is gitignored.
- [x] **Notify-on-failure** — on any failed node, runs an `on-failure.mmd` hook if present (passing `error`/`graph`/`failed_nodes` as inputs) and posts a desktop notification (`osascript`). Verified.

Auth stays keyless: Composio via `composio login`, LLM via the `claude` CLI.

### 13.1.1 Authoring, state & caching (added)

- [x] **Config SSOT** — `<graphs-dir>/config.json` (flat object) supplies `{ctx.*}` defaults (constants like `sheet_id`). Gitignored; `config.example.json` is the template.
- [x] **Discovery + fail-fast validation** — `psflow-run --list` shows graphs + descriptions; before running, the runner scans the graph for `{ctx.KEY}` references and errors with `missing required input(s): …` instead of a mid-render failure.
- [x] **Cross-run state** — per-graph `<state-dir>/<graph>.json`. Precedence config < state < `--input`. Reads merge into `{ctx.*}`; on success, outputs prefixed `save_` persist (prefix stripped). Verified with a counter that survives across runs. Also wires a context-aware `rhai` so scripts read `ctx_get(ctx, "key")`.
- [x] **Record/replay + caching** — the `composio` handler caches responses keyed by (tool, args, dry_run) when `PSFLOW_TOOL_CACHE_DIR` is set. Runner flags: `--replay` (use any cached, record on miss — offline dev), `--cache [--cache-ttl secs]` (use within TTL). Verified: replay served a marker response in 0.3 ms vs 2.8 ms live, no network. Caution: caching applies to all Composio calls including writes — enable deliberately.

The precedence and merge order is: `config.json` < cross-run state < `--input`, evaluated before fail-fast validation so state/config can satisfy required inputs.

### 13.1.2 Trigger bridge (event-driven, added)

Composio triggers are usable for single-user without a public webhook: `composio dev listen --json` streams trigger events to stdout locally. `psflow-run --listen <handler-graph>` wraps it, parsing each event line and running the handler graph once per event with the event JSON injected as `{ctx.event}`. Cross-run state gives dedup; run-history + notify wrap each dispatch.

- Setup (one-time): `composio dev init`, then `composio dev triggers create <slug>` (binds a connected account + config, e.g. a sheet + A1 range), `composio dev triggers enable`.
- Run: `psflow-run --listen on-event [--trigger-slug X] [--toolkits Y]`.
- Filters (`--trigger-slug`, `--toolkits`, `--max-events`) pass through; the listen command is overridable via `PSFLOW_LISTEN_CMD` (used for testing).
- Triggers are **poll-type** (latency in minutes; managed apps ~15-min floor). Non-JSON lines (banners) are skipped.
- Verified with a simulated event stream: 2 events dispatched, banner skipped, event reached the handler as `{ctx.event}`, run records written.

**Receive path for personal accounts (no org).** `composio dev listen` requires a developer project, which needs an org; a personal workspace returns `Failed to list org projects: HTTP 404` (confirmed on CLI 0.2.30 — not a version/tier/PATH issue). The org-free alternative, per the docs: create the trigger via the **dashboard** (or SDK), and receive via the SDK websocket `composio.triggers.subscribe()` — needs only `COMPOSIO_API_KEY`, no dev project. `scripts/triggers_listen.mjs` (TypeScript/`@composio/core`, run as Node ESM — no build step) implements this, printing each event as a JSON line that the existing `--listen` loop consumes via `PSFLOW_LISTEN_CMD`. Run with `just triggers <handler> <ti_id>`. Trade-off: this receive path is **not keyless** (the SDK needs the api key) and requires `npm i @composio/core`. Pending a live end-to-end run.

Composio features still unused (smaller, optional): `composio proxy` (un-wrapped provider APIs with managed auth → a `composio_proxy` handler mode), `execute -p` parallel/batch (a `multi` handler mode), `composio run` (inline TS/JS agent runtime — redundant with psflow), and `dev logs` (full request/response forensics beyond the captured `log_id`).

### 13.2 Scheduling — recipe

The `ergon-scheduler` fires cron shell jobs that survive reboot (launchd). Schedule a named graph with a `schedule_create` shell job whose command sets PATH (for `composio`) and uses absolute paths for the binary + `--graphs-dir`/`--runs-dir`. Cadence and target graph are chosen per job; no standing job is created by default.

### 13.3 Follow-ups — done

- [x] **Numeric inputs via templates** — the `composio` handler coerces whole-value tokens (`"max_results": "{ctx.n}"`) whose rendered text parses as a non-string JSON scalar to that type, so a numeric input reaches an integer-typed tool arg. Verified (`--input max=2` accepted by schema validation). Caveat: a string input that happens to look numeric also coerces.
- [x] **`{ctx.key}` in `llm_call` prompts** — the runner constructs `llm_call` via `LlmCallHandler::with_context` with a blackboard seeded from the runtime inputs, so prompts read `{ctx.key}`. Verified (`--input word=PONG` echoed by the LLM).
- [x] **Binary install** — `just install` runs `cargo install --path . --bin psflow-run`; installed to `~/.cargo/bin/psflow-run`. Scheduled jobs should pass absolute `--graphs-dir`/`--runs-dir` and set PATH for `composio`.

## 14. Out of scope (recorded, not tasks)

- Hosting psflow workflows as registered Composio tools — no server-side registration API; would require a hosted SDK shim.
- Composio auth-config (`ac_…`) provisioning inside psflow — one-time dashboard/SDK artifact.
- Secret storage / rotation / lifecycle — host concern, unchanged from the existing auth design.
- Replacing the executor loop with an MCP client or Composio agent loop — explicitly rejected; MCP is output-only.
- Long-running builds on the sandbox — the ~180s ceiling rules them out.
