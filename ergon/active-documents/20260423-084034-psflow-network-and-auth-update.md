20260423-084034-psflow-network-and-auth-update.md

# psflow Network & Auth Update — Task Tracker

## 1. Context

1.1. Proposal originated from a downstream consumer project that plans to build on psflow and needs richer network I/O plus a graph-level auth concern.

1.2. This doc folds the consumer proposal together with an audit of current psflow state so the work can be scheduled and tracked.

1.3. Working task tracker — not a design doc. Design details belong in per-workstream design notes once started.

## 2. Scope & Non-Scope

2.1. In scope: WebSocket handler, graph-level auth strategies, streaming HTTP response to disk, declarative schema validation (HTTP + WS), optional `poll_until` node, HTTP handler gap-fill.

2.2. Non-scope (explicit):
- Secret storage or resolution — host provides an opaque secret resolver.
- Capture-to-graph synthesis (recording live traffic into graph definitions).
- MCP exposure of graphs.
- Service-specific schemas, fixtures, or adapters.

## 3. Current-State Summary (Audit)

| Item | Status | Notes |
|---|---|---|
| WebSocket handler | MISSING | `tokio_tungstenite` is a dependency but only used by the debug server, not as a node handler. |
| Graph-level auth | MISSING | `GraphMetadata` has no auth field; HTTP handler has no credential hook. Headers use `{key}` template interpolation only. |
| Streaming HTTP to disk | MISSING | HTTP handler buffers the full response body. File I/O handlers exist but are not wired to HTTP. |
| Response schema validation | MISSING | No JSON Schema dependency; no validation path in HTTP handler. |
| Polling | PARTIAL | `DelayHandler`, loop subgraphs, and fixed/exponential retry backoff exist. No dedicated `poll_until` node. |
| HTTP handler — general | PARTIAL | Has: per-request timeout (default 30s), non-2xx passthrough (status returned), header template interpolation. Missing: multipart/upload, redirect policy, HTTP-wired retry, configurable non-2xx failure mode, auth hook. |

## 4. Key Observation — Dependency Shape

4.1. Proposal claims auth blocks only items 3 and 4, but every remaining HTTP/WS item lands inside handler config. Auth therefore effectively blocks items 3, 4, 5, and 6.

4.2. Touching handler config twice is wasted motion. Land the auth hook surface first; build everything else against it.

## 5. Workstreams

### 5.1. Auth Strategy Layer (architecturally novel)

- Status: not started.
- Goal: graph declares named auth strategies; network handlers opt in via `auth: <strategy-name>`; runtime applies strategy at request-build time via a typed injection hook.
- Tasks:
  - [ ] Extend `GraphMetadata` with an auth-strategies section.
  - [ ] Define the strategy abstraction (trait object vs. enum — decide).
  - [ ] Ship built-in strategies: static header, bearer token, cookie jar, HMAC-signed request.
  - [ ] Define the host-supplied secret resolver interface (opaque — psflow does not care where secrets live).
  - [ ] Wire injection hook into HTTP request build path.
  - [ ] Document strategy authoring for out-of-tree plugins.
- Open questions:
  - Trait object vs. enum for strategy dispatch — pick based on whether host plugins need to register their own strategies.
  - Scope of cookie jar persistence (per-run vs. per-graph vs. per-strategy instance).

### 5.2. HTTP Handler Audit Gap-Fill (in place, no fork)

- Status: partial — see audit row.
- Goal: close the gaps in the existing HTTP handler so downstream consumers can express real-world requests.
- Tasks:
  - [ ] Multipart body support (file upload).
  - [ ] Per-request redirect policy config.
  - [ ] HTTP-wired retry policy (attempts, backoff, retry-on predicate) — distinct from node-level `exec.retry`; decide whether to share `RetryConfig`.
  - [ ] Configurable non-2xx behavior (fail node vs. pass through with status/body).
  - [ ] Header injection hook compatible with the auth strategy layer (5.1).
- Open questions:
  - Reuse existing `RetryConfig` or introduce an HTTP-scoped variant with retry-on-status and retry-on-predicate fields.
  - Do we keep the existing passthrough default, or flip it to fail-on-non-2xx and make passthrough opt-in.

### 5.3. Schema Validation (shared HTTP + WS)

- Status: not started.
- Goal: declarative JSON Schema validation on HTTP response bodies and WS frames; configurable failure mode.
- Tasks:
  - [ ] Pick and add a JSON Schema crate.
  - [ ] Define the schema-attach config surface (inline vs. referenced).
  - [ ] Failure modes: `fail` (fail the node) vs. `passthrough` (attach `validation_error` field to output).
  - [ ] Wire into HTTP handler on response.
  - [ ] Reuse the same validator in the WS handler (5.5) on each frame.
- Open questions:
  - Where schemas live: inline in the node, in graph metadata, or loaded from disk.
  - Whether to cache compiled schemas per graph run.

### 5.4. Streaming HTTP Response to Disk

- Status: not started.
- Goal: stream large HTTP response bodies directly to a file; output is the final on-disk path.
- Preference: extend HTTP handler with `body_sink: File { path_template }` rather than adding a separate download node, to avoid duplicating HTTP config surface.
- Tasks:
  - [ ] Add `body_sink` config variant.
  - [ ] Implement streaming write path (avoid full-body buffer).
  - [ ] Interpolate `path_template` from upstream context using the existing template resolver.
  - [ ] Emit final path as node output.
  - [ ] Decide interaction with schema validation (5.3) — validating a streamed body conflicts with the sink; document the rule.
- Open questions:
  - Behavior when the target path exists (overwrite, fail, suffix).
  - Whether to also expose content-length / bytes-written as sibling outputs.

### 5.5. WebSocket Node Handler

- Status: not started.
- Goal: open a WS connection, optionally send an init frame, emit received frames as a stream, terminate on a predicate or external cancellation.
- Tasks:
  - [ ] New handler type; connection lifecycle managed by the node.
  - [ ] Optional init frame config.
  - [ ] Frame emission as a stream output into the graph.
  - [ ] Termination predicate over frame content + external cancellation signal.
  - [ ] Per-frame schema validation via 5.3.
  - [ ] Auth via 5.1 (handshake headers, signed subprotocols, etc.).
- Open questions:
  - How streaming outputs compose with existing node I/O (fan-out to downstream vs. buffered list).
  - Reconnect policy — in scope for v1 or deferred.

### 5.6. `poll_until` Node (conditional)

- Status: COMPLETE — narrow-scope build landed.
- Goal: invoke a subgraph, sleep, re-invoke, terminate on a predicate over subgraph output or max attempts.
- Tasks:
  - [x] Author a polling example using existing primitives (`DelayHandler` + loop subgraph + retry) first.
  - [x] Evaluate ergonomics; decide go/no-go.
  - [x] Narrow-scope design landed: single predicate + max-attempts + fixed delay + subgraph reference, nothing more.
- Outcome:
  - Compose-first evaluation (see `ergon/active-documents/20260423-165256-polling-compose-first-findings.md`) flagged three seams: manual attempt counter, no rhai→blackboard write path (blocks exponential backoff via composition), and dual-path termination with no unified exit signal. Recommendation: BUILD-BUT-NARROWER.
  - Implementation: `src/handlers/poll_until.rs`. Four config keys (`graph`, `predicate`, `max_attempts`, `delay_ms`) and three output keys (`attempts_used`, `timed_out`, `output`). Predicate is a cached Rhai AST; subgraph dispatch delegates to `SubgraphInvocationHandler`; delay uses `tokio::time::sleep` racing against cancellation. On cap without match the node succeeds with `timed_out=true` — callers branch on it.
- Explicit deferrals (follow-up candidates if demand surfaces):
  - Exponential / custom / jittered backoff.
  - Per-attempt timeout (subgraph itself can enforce via its own handler timeouts).
  - Retry-on-subgraph-error (wrap in a node-level retry handler if needed).
  - Fail-on-cap toggle (always succeeds with `timed_out` today).
  - Initial / startup delay (first attempt fires immediately by contract).
  - Custom cancellation policy beyond the ambient cancel token.

## 6. Landing Order & Dependencies

6.1. Sequence:
1. Auth Strategy Layer (5.1)
2. HTTP Handler Audit Gap-Fill (5.2)
3. Schema Validation (5.3)
4. Streaming HTTP Response to Disk (5.4)
5. WebSocket Node Handler (5.5)
6. `poll_until` evaluation, then optional build (5.6)

6.2. Rationale: auth touches handler config for both HTTP and WS; landing it first avoids revisiting every handler twice. Gap-fill next because it is the largest pure-HTTP surface. Schema validation before streaming so the validation wiring is in the handler before streaming introduces a body-sink branch. WS after HTTP so it can reuse auth, schema validation, and header conventions. Polling last and conditional.

6.3. Cross-cutting: every network-touching workstream (5.2, 5.4, 5.5) must consume the auth hook from 5.1 and the validation hook from 5.3 rather than growing its own.

## 7. Out of Scope (Restated)

- [ ] Secret storage / resolution — host responsibility via opaque resolver.
- [ ] Capture-to-graph synthesis.
- [ ] MCP exposure of graphs.
- [ ] Service-specific schemas and fixtures.

## 8. Open Cross-Cutting Decisions

8.1. Strategy dispatch mechanism (trait object vs. enum) — affects extensibility for out-of-tree auth strategies.

8.2. Retry config unification — one `RetryConfig` shared between node-level and HTTP-level, or two distinct types.

8.3. Schema source of truth — inline vs. graph metadata vs. external file reference.

8.4. Streaming + validation interaction — documented rule for which wins when both are configured.
