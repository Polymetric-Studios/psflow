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

### 2.2 Non-goals

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

The whole of Phase 1 rests on one synchronous call. Facts the handler depends on:

- Endpoint: tool-execute by slug under the v3 tools API; authenticated with an `x-api-key` header (not Bearer/OAuth on our side).
- Request carries: `user_id`, `arguments` (object), an explicit toolkit `version`, and an optional `connected_account_id` (auto-resolved from `user_id` when omitted).
- Response is a fixed envelope: a boolean success flag (spelled `successful`), a tool-specific `data` object, an `error` string, and a `log_id`.
- Behaviour is request/response — no polling, no webhook for execution itself.

### 4.1 Hard facts that shape the design

- **Version default is the base version, not latest.** Omitting `version` silently exposes fewer fields than the dashboard shows. The handler must pin a version (config-defaulted), since downstream steps consume typed `data`.
- **Rate limit is org-global on a rolling ~10-minute window.** A fan-out graph can exhaust the whole org quota. Backoff must be central, not per-node.
- **Auth header is a single static key.** The interim `http`-handler path needs only one header — no OAuth wiring — making a no-new-code prototype viable.
- **Proxy execution exists** for un-wrapped APIs: reuse a stored connected account, never set `Authorization`, same-domain constraint, no multipart.

## 5. Phase 1 — generic `composio` handler

Goal: any Composio tool becomes a psflow step via `handler: composio` + a tool slug.

Config surface (annotations): toolkit/tool slug, `user_id` (templated), `arguments` (templated object), pinned `version`, optional `connected_account_id`, api-key reference resolved from the secret layer.

- [ ] **Decide handler scope** — one generic `composio` handler keyed by slug vs. a thin family. Recommendation: single handler; the slug selects direct-execute, meta-tools, proxy, and sandbox modes.
- [ ] **Interim no-build path** — author a runnable `.mmd` using the existing `http` handler against the execute endpoint with the `x-api-key` header, to validate end-to-end before any Rust lands. Gate: a Composio account with a linked connected account exists.
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

## 7. Phase 3 — triggers / event entry

Goal: a verified Composio event starts a psflow workflow.

Delivery is a webhook (production) or SDK websocket subscribe (dev). The payload carries the trigger slug, trigger id, connected-account id, auth-config id, and `user_id` in metadata, plus an event-specific `data` block. The same channel also delivers connected-account-expired and trigger-disabled events.

- [ ] **Svix webhook verification middleware** — read the three webhook headers, recompute HMAC-SHA256 over `id.timestamp.rawbody`, base64, constant-time compare, enforce ~300s skew. Verify on raw bytes before JSON parse. A Svix-compatible verifier can be reused.
- [ ] **`composio_trigger` node** — declares toolkit + trigger slug + `user_id`; on event, seeds the event-driven executor with `metadata.user_id`, connected-account id, and `data` as run context.
- [ ] **Workflow dispatch** — map `metadata.trigger_slug` to the target workflow.
- [ ] **Lifecycle events** — handle trigger-disabled (mark the node dead) and connected-account-expired (route to re-auth from §6).
- [ ] **Trigger provisioning** — resolve the create-trigger SDK signature and `ti_…` provisioning call (not on the fetched doc pages; see §12) and wire a provisioning step or document a manual dashboard prerequisite.

## 8. Phase 4 — LLM tool-use

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

All reachable through the same handler by slug; each is a high-leverage psflow capability.

- [ ] **Batch / fan-out** — a single node executes N tool calls and returns ordered results, mapping directly onto the render fan-out + accumulator pattern. Evaluate offloading large payloads to remote storage to keep them out of the transport.
- [ ] **Dynamic tool search** — late-bound slug resolution from intent (also referenced in §8.3).
- [ ] **Sandboxed shell / workbench** — off-host shell and Python with state threaded by session id; lets psflow run compute steps without host shell access. Respect the hard ~180s per-call ceiling; note workbench billing is imminent before depending on it.
- [ ] **MCP as an output surface** — expose a curated, allow-listed Composio MCP server to the agents psflow orchestrates, while the engine's own nodes stay on REST. Keep the boundary: engine → REST; orchestrated agents → MCP.

## 11. Reusable graph templates

Ship as starter `.mmd` files; they are the two canonical shapes the cookbooks reduce to.

- [ ] **Event-trigger → enrich → llm → act** — trigger, extract, LLM analyze, then a Composio action (e.g. label/apply). Covers the inbox-labeler and PR-review shapes.
- [ ] **Scheduled fan-out digest** — scheduled trigger, parallel Composio fetch nodes, one LLM digest, one Composio post. Covers the background-agent shape.
- [ ] **Selling-point note** — these cookbooks bound their agent loop with a step cap; psflow replaces that loop with explicit nodes, giving deterministic control. Capture this as positioning.

## 12. Open questions / to verify before coding

- [ ] Confirm exact optional-field spellings on the execute request body against the live API reference (the spec page is JS-rendered and could not be fetched verbatim).
- [ ] Confirm the batch meta-tool's maximum concurrency (undocumented) before relying on wide fan-out.
- [ ] Obtain the create-trigger SDK signature and `ti_…` provisioning call from the SDK reference (absent from the fetched doc pages).
- [ ] Confirm whether toolkit `version` is per-tool or per-toolkit for the slugs psflow will use first.

## 13. Out of scope (recorded, not tasks)

- Hosting psflow workflows as registered Composio tools — no server-side registration API; would require a hosted SDK shim.
- Composio auth-config (`ac_…`) provisioning inside psflow — one-time dashboard/SDK artifact.
- Secret storage / rotation / lifecycle — host concern, unchanged from the existing auth design.
- Replacing the executor loop with an MCP client or Composio agent loop — explicitly rejected; MCP is output-only.
- Long-running builds on the sandbox — the ~180s ceiling rules them out.
