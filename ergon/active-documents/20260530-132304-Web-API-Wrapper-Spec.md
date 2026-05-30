
20260530-132304-Web-API-Wrapper-Spec.md

# Web API Wrapper — Corrected Spec & Implementation Plan

## 1. Preamble

### 1.1 Context

Part of the effort to wrap undocumented web-UI-only services (starting with Freepik AI generation) as executable psflow graphs consumable via MCP; this document supersedes the initial spec by re-grounding it against psflow's actual current handlers, auth subsystem, and state model.

### 1.2 Purpose

Correct the initial spec's assumptions against what psflow already provides and lay out the implementation plan, so the work partitions cleanly into "author the Freepik graph" (almost all of it) versus "extend psflow" (near zero).

## 2. Scope

### 2.1 In scope

- Freepik AI generation wrapped as a psflow service graph, exposed via MCP.
- A corrected mapping of the spec's five node-type primitives onto psflow's existing handlers and auth subsystem.
- The `cookie_jar` auth path for Freepik, including how the session cookie is sourced.
- Reuse of existing psflow primitives wherever they cover the need; only genuinely missing pieces become psflow work.
- A pattern that subsequent same-shape services can follow.

### 2.2 Out of scope

- Distribution to others, multi-user auth, rate-limit enforcement, usage analytics, content-licensing enforcement.
- Freepik's official paid API.
- The LLM-assisted graph synthesizer (deferred; service #1 is hand-authored).
- Drift detection / fixture replay as a general psflow capability (deferred; fixtures are still captured for later use).

## 3. Status

Spec corrected against psflow's current state; no Freepik graph authored yet. The headline finding: all five spec primitives and the required `cookie_jar` auth strategy already ship in psflow — this project is overwhelmingly graph authoring, not psflow build-out. The design questions are now resolved (§5); what remains is recon and authoring (§9.1).

## 4. Body

### 4.1 Architecture

The pipeline is unchanged from the initial spec; only the realization of each stage is corrected.

- A HAR file is captured manually via browser DevTools during a recon session.
- An optional LLM-assisted synthesizer produces a draft graph from the HAR. Deferred — service #1 is authored by hand.
- The service graph (`graph.mmd` plus schema and auth declarations) is authored once and versioned in git, using the psflow node types below.
- The psflow runtime executes the graph and exposes its operations through one MCP server.

### 4.2 Node-type mapping (corrected)

The initial spec asked which of its five primitives exist in psflow, which are trivial extensions, and which are genuinely new. Answer: four exist as first-class handlers, and the fifth is covered in substance by an existing handler. Nothing is genuinely new.

- **`http_request` — exists, full.** Handler is the `http` node type. Provides method, URL templating, headers, multipart file upload (`config.multipart.fields` and `config.multipart.files` with path or inline bytes), typed body, `timeout_ms`, `redirect` policy, a `retry` policy (backoff, multiplier, `retry_on`), `fail_on_non_2xx`, JSON-Schema response `validation` in fail-or-passthrough modes, and auth injection via `config.auth`. The only shortfall is cosmetic: query params are templated into the URL rather than supplied as a dedicated `query_params` object.

- **`websocket_subscribe` — exists, full.** Handler is the `ws` node type. Provides `init_frames` (the subscription payload), `subprotocol`, a content-based termination predicate (`terminate.on_predicate`, a Rhai expression over the frame and its index), `max_frames`, `timeout_ms`, per-frame schema `validation`, external cancellation, and auth injection on the handshake. No gaps against the spec.

- **`polling_loop` — exists.** Handler is the `poll_until` node type. Invokes a named subgraph repeatedly, with a Rhai `predicate` over the subgraph output and attempt count, `max_attempts`, and a fixed `delay_ms`. Timeout is a soft, non-failing stop. The only gap is the absence of exponential backoff or jitter, which is an intentional design choice and irrelevant to Freepik.

- **`auth_strategy` — exists, full.** A complete auth subsystem implements `cookie_jar`, `bearer`, `hmac` (the spec's `signed_header`), and `static_header`. Auth is a graph-level declaration applied to `http` and `ws` nodes via each node's `config.auth`. Detail in §4.3.

- **`file_output` — exists in substance, not as a named node.** Downloading a possibly-signed, time-limited URL to disk is done by the `http` node with a `body_sink.file` configuration: the request URL (`config.url`) is the signed download source, and `body_sink` streams the response to a templated local path with overwrite and create-parents controls, emitting the final path and bytes written. The separate file-I/O handlers (`Read`, `Write`, `Glob`) write in-context content to disk, not URL downloads — useful for sidecar metadata, not the image bytes. Fan-out over multiple image URLs is not built into a single node; it is expressed with the `ParallelLoop` subgraph directive (recently landed) iterating the URL list, each iteration an `http`-plus-`body_sink` download.

### 4.3 Auth (Freepik specifically)

Freepik uses the `cookie_jar` strategy, which is exactly the shape Freepik needs.

- Cookies are injected as a `Cookie` header from per-run jar state, and the jar absorbs `Set-Cookie` mutations from responses (round-trip).
- Domain filtering is enforced (suffix-match semantics, case-insensitive, port-stripped), so cookies do not leak across domains.
- The strategy supports the WebSocket handshake, so the same auth applies if completion turns out to be WS-based.
- Secrets are named logical references resolved at runtime by a host-provided resolver; env, keychain, and 1Password are all host-pluggable, and secrets are never inlined in graph source.
- The jar is seeded by a login-export helper: after a manual browser login, a small helper script captures the session cookies and writes them to a persisted jar file (under the service directory or a secrets path) that the graph loads at run start.
- No refresh: on session expiry the jar yields no valid cookie and the request fails. The graph surfaces a clear "session expired, re-authenticate" condition; the human re-logs-in manually.

The graph declares its strategy at graph scope (strategy type plus the secret role-to-logical-name mapping) and each request node names that strategy in `config.auth`. Load-time validation rejects an undeclared strategy reference, a missing required secret role, or a WS node bound to a strategy that cannot sign a handshake.

### 4.4 State and data flow

The spec's second question — whether psflow's context model cleanly supports `job_id` → image URLs → files passing — resolves to yes.

- Primary dataflow is typed port edges: an edge maps a source node's output port to a target node's input port, and port types are checked at graph-load time. A shared global blackboard exists alongside for name-addressable lookups when an edge is inconvenient.
- Handlers reference upstream values via single-brace interpolation (`{inputs.job_id}`, `{ctx.key}`), not double-brace template syntax — a point worth correcting in any mental model carried from the initial spec.
- The chain maps directly: `submit_generation` declares a `job_id` output, `wait_for_completion` takes `job_id` and produces a URL list, `download_results` takes the URL list. Edges connect the named ports.
- Two rough edges to plan around: ports do not auto-rename, so mismatched names are wired explicitly on both ends; and edge-level typing is structural (port names and coarse types), not payload-schema validation — payload schemas are enforced inside the `http`/`ws` nodes via their `validation` config, which is the right place for them.

### 4.5 The Freepik graph

Three top-level flows exposed as callable operations.

- **`generate`** — `submit_generation` (`http` POST to the generate endpoint) extracts a `job_id`; `wait_for_completion` (`poll_until` against a status endpoint — the chosen default; WS is a deferred fallback) extracts image URLs on completion; `download_results` writes the URL list to a blackboard key and runs a `ParallelLoop` `ForEach` over it, each iteration an `http`-plus-`body_sink` download with the URL exposed as `loop.item` and parallelism capped by `max_concurrent`; the operation returns the local file paths.
- **`list_models`** — a single `http` request to the model catalog, returning available models with their parameter schemas so callers (including Claude via MCP) can pick a valid model and know its params.
- **`history`** — an `http` GET against the history endpoint with pagination, returning past generations with metadata, enabling "regenerate from a past prompt" without re-querying the Freepik UI.

### 4.6 MCP exposure

One MCP server hosts all psflow service graphs (not one server per service). Tools are named `<service>.<operation>` — `freepik.generate`, `freepik.list_models`, `freepik.history`. Tool parameter schemas are auto-derived in structure from each graph's declared input ports (single source of truth, no drift), with tool and parameter descriptions hand-written so the LLM caller gets clear guidance on prompt, model, and params; responses are the graph's output, which for `generate` is the set of local file paths.

## 5. Decisions

- **Reuse `http` + `body_sink` for URL-to-disk download rather than build a dedicated `file_output` terminal node.** Rationale: the capability already exists and a signed URL is just the request URL. Rejected alternative — a new terminal handler — adds surface for no functional gain; a thin readability alias remains optional polish, not a blocker.
- **Express multi-image fan-out with `ParallelLoop`, not an in-node N-URL download.** Rationale: the fan-out primitive recently landed and composes cleanly with `http`+`body_sink`. Verified against the landed runner — `ParallelLoop` with a `ForEach` config reads a JSON array from a blackboard key and runs the body subgraph once per item in a snapshot child context, exposing the item as `loop.item` with an optional `max_concurrent` cap. The one wrinkle: fan-out items arrive via the blackboard `loop.item`, not a typed edge, so the download node reads `{loop.item}` rather than an input port. Rejected alternative — extending the download node to accept a URL list — duplicates loop semantics that already exist.
- **Use `cookie_jar` with a host `SecretResolver`, no refresh.** Rationale: matches Freepik's manual-login, expire-and-re-login reality. Rejected alternative — a bearer refresh hook — is unimplemented and unneeded here.
- **Seed the cookie jar with a login-export helper script writing to a persisted jar file.** Rationale: clean, repeatable, and decoupled from the recon HAR; matches the original spec's preferred "export via helper script" path. Rejected alternatives — reusing the recon HAR's cookies (couples secrets to a debug artifact, awkward to refresh) and reading the browser's cookie store at runtime (fragile, browser- and OS-specific).
- **Use `poll_until` for `wait_for_completion` (polling-first); keep WS as a deferred fallback.** Rationale: polling always works, is the simplest to author and debug, and fits personal single-user use; the `ws` handler stays available if recon shows completion latency or request volume justifies it. Rejected alternative — authoring both paths for service #1 — doubles the surface for no proven need.
- **Auto-derive MCP tool param-schema structure from input ports; hand-write the descriptions.** Rationale: structure stays single-sourced from the graph (no drift) while the LLM caller still gets well-worded guidance. Rejected alternatives — fully auto-derived (terse, port-metadata-only descriptions) and fully hand-written schemas (drift from the graph as it changes).
- **Defer a dedicated `query_params` object on `http`; template params into the URL for now.** Rationale: YAGNI — the existing `http` node already templates the URL, keeping the project at near-zero psflow work. Rejected alternative — adding the structured map up front — extends psflow before recon proves it necessary. Revisit per §8.
- **Hand-author Freepik first; defer the synthesizer.** Rationale: one graph is faster to write than a synthesizer is to build, and authoring it surfaces the patterns the synthesizer would need to learn.

## 6. Open questions

_None._

## 7. Usage

Callers (including Claude via MCP) invoke the service through three tools. `freepik.generate(prompt, model, params, reference_image?)` returns local file paths to the generated images. `freepik.list_models()` returns the model catalog with each model's parameter schema. `freepik.history(limit, offset)` returns past generations with metadata. On an expired session, `generate` fails with a clear re-authenticate condition rather than a silent error.

## 8. Known limitations

- The cookie jar is per-run and in-process; there is no built-in file persistence. The host exports and seeds it. Revisit when cross-run cookie reuse without re-login becomes necessary.
- No bearer refresh hook exists. Revisit when a wrapped service requires token refresh.
- The `hmac` strategy cannot sign a WebSocket handshake. Revisit when a service needs a signed WS handshake.
- `poll_until` uses a fixed delay with no backoff or jitter. Revisit when a wrapped service rate-limits aggressively enough to require backoff.
- `http` has no dedicated `query_params` object; params are templated into the URL. Revisit when complex query encoding makes templating unwieldy.
- The graph synthesizer is not built. Revisit when onboarding service #2 and beyond.

## 9. Plan

### 9.1 Planned tasks

- [ ] Recon Freepik through DevTools and capture a HAR covering generate, list_models, and history; determine whether completion is WS or polling.
- [ ] Scaffold the `services/freepik` directory: graph, auth declaration, schemas folder, fixtures folder, and a recon/gotchas README. depends-on:recon
- [ ] Build the login-export helper that captures session cookies post-login into a persisted jar file, then declare the `cookie_jar` strategy wired to a host `SecretResolver`. depends-on:scaffold
- [ ] Author the `generate` flow: `http` submit extracting `job_id` → `poll_until` wait extracting URLs → `ParallelLoop` `ForEach` download via `http`+`body_sink`. depends-on:auth
- [ ] Author the `list_models` single-`http` flow. depends-on:scaffold
- [ ] Author the `history` flow with pagination. depends-on:scaffold
- [ ] Define JSON schemas for submit request/response and status frame; wire them into the relevant nodes' `validation` config. depends-on:generate
- [ ] Capture fixtures from the recon session for later drift checks. depends-on:recon
- [ ] Expose the service via MCP: host the graph, name tools `freepik.<operation>`, auto-derive param-schema structure from declared input ports, and hand-write the tool/param descriptions. depends-on:generate
- [ ] Run an end-to-end `generate` test and verify the session-expired failure path surfaces the re-authenticate condition. depends-on:mcp
- [ ] (Optional psflow polish, only if recon shows the need) add a `file_output` alias over `http`+`body_sink` for readability. The `query_params` object is deferred per §8.

### 9.2 Audit fixes

_None._

### 9.3 Phased plan

_None._

## 10. Calibration notes

The first-pass capability audit mis-scored `file_output` as a high-severity gap by conflating the download source (`config.url`, which can be the signed URL) with the local sink path (`body_sink.file.path`). Corrected to "exists in substance" in §4.2. The lesson generalizes: when auditing a handler, separate where data comes from versus where it goes before declaring a gap.

## 11. History

### 11.1 Already landed

These psflow capabilities already shipped and are what make this project minimal: the `http`, `ws`, and `poll_until` handlers; the full auth subsystem including `cookie_jar` with domain filtering and the `Set-Cookie` round-trip; and the `ParallelLoop` subgraph directive and runner used for download fan-out.

### 11.2 Deferred / superseded

The initial Web API Wrapper spec (`20260530-111310-web-api-wrapper-spec.md`) is superseded by this corrected version. Its open questions about which node types exist and whether the state model supports inter-node passing are answered in §4.2 and §4.4.

## 12. Appendix

_None._

## 13. Related

- `20260530-111310-web-api-wrapper-spec.md` — the superseded initial spec.
- psflow handler sources: `src/handlers/http.rs`, `src/handlers/websocket.rs`, `src/handlers/poll_until.rs`, `src/handlers/file_io.rs`.
- psflow auth subsystem: `src/auth/`.
- psflow execution and state model: `src/execute/`, `src/blackboard/`, `src/graph/`.
- The host-integration quickstart and Mermaid annotation reference added in recent commits.
