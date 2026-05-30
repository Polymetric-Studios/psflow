# Web API Wrapper — Project Spec

## Purpose

Wrap undocumented web-UI-only services (starting with Freepik AI generation) as executable psflow graphs, consumable via MCP. Personal use, single user, solo maintenance.

## Scope boundaries

- **In:** Freepik AI generation via reverse-engineered web API. Support for subsequent services with the same pattern.
- **Out:** Distribution to others, multi-user auth, rate limit enforcement, usage analytics, content licensing enforcement, Freepik's official paid API.
- **Deferred:** Drift detection / fixture replay as a general psflow capability. Useful later but not blocking first working wrapper.

## Architecture

```
┌───────────────────────────────────────────────────────┐
│  HAR file (captured manually via browser DevTools)    │
└───────────────────────────────────────────────────────┘
                         │
                         ▼
┌───────────────────────────────────────────────────────┐
│  Synthesizer (LLM-assisted, produces draft graph)     │
│  ─ optional; service #1 authored by hand              │
└───────────────────────────────────────────────────────┘
                         │
                         ▼
┌───────────────────────────────────────────────────────┐
│  Service graph: spec.mmd + endpoints/*.yaml           │
│  ─ authored once, versioned in git                    │
│  ─ uses psflow node types below                       │
└───────────────────────────────────────────────────────┘
                         │
                         ▼
┌───────────────────────────────────────────────────────┐
│  psflow runtime executes the graph                    │
│  ─ exposes operations via MCP server                  │
└───────────────────────────────────────────────────────┘
```

## Node types needed from psflow

These are the primitives the project needs. Some may exist in psflow already; some may need to be added. The purpose of speccing them is so you can diff against what psflow provides.

### 1. `http_request`

Execute an HTTP request and produce a typed response.

- **Inputs:** method, URL template (with param interpolation), headers (with auth injection hook), query params, body (typed, possibly multipart for file upload).
- **Outputs:** status code, headers, body parsed per declared response schema.
- **Errors:** transport errors, non-2xx responses (configurable: fail vs pass-through for status inspection), schema validation failures.
- **Config:** timeout, redirect policy, retry policy.

### 2. `websocket_subscribe`

Open a WebSocket connection, optionally send a subscription message, and emit received frames as a stream.

- **Inputs:** URL, initial subscription payload, auth injection hook.
- **Outputs:** stream of parsed frames (each validated against a declared frame schema).
- **Termination:** predicate on frame content (e.g., "emit until status=complete"), or externally cancelled.
- **Errors:** connection errors, frame schema violations, unexpected close.

### 3. `polling_loop`

Repeatedly execute a downstream operation until a predicate is satisfied.

- **Inputs:** operation to invoke, interval, max attempts, termination predicate.
- **Outputs:** final operation output when predicate matches.
- **Errors:** timeout, max attempts exceeded, downstream errors.

### 4. `auth_strategy` (not a node, a graph-level concern)

Selects how auth is applied to all `http_request` and `websocket_subscribe` nodes in the graph. Strategies for v1:

- `cookie_jar` — load cookies from persisted store, inject into requests, persist mutations back
- `bearer_token` — load token from secret store, inject as `Authorization: Bearer`, support refresh hook
- `signed_header` — compute HMAC or similar per request from a secret

Secrets reference named entries (`$FREEPIK_SESSION_COOKIE`) resolved at runtime from env, keychain, or 1Password — never inlined.

### 5. `file_output`

Terminal node: download a URL (possibly signed/time-limited) to a configured output directory with a filename template.

- **Inputs:** URL, filename template (with interpolation from upstream context), output directory.
- **Outputs:** final file path.
- **Errors:** transport, disk.

### Orchestration primitives assumed to exist in psflow

Graph-level flow control (sequencing, parallelism, error propagation, cancellation), context/state passing between nodes (e.g., `job_id` from submit flows to status polling), and the execution model itself.

## Spec format

Each wrapped service is a directory containing:

```
services/freepik/
  graph.mmd                 # psflow graph with @NodeID annotations
  auth.yaml                 # auth strategy + secret refs
  schemas/                  # JSON Schema files referenced by node configs
    submit_request.json
    submit_response.json
    status_frame.json
    ...
  fixtures/                 # captured examples, used for drift checks later
    submit_example_1.json
    ...
  README.md                 # notes on recon, gotchas, manual login steps
```

The `.mmd` describes the graph; `@NodeID` annotations carry node-specific config. Follows whatever psflow's convention is.

## The Freepik graph, specifically

Three top-level flows the graph exposes as callable operations:

### `generate`

```
start
  ↓
submit_generation           # http_request POST /ai/generate
  ↓ (extract job_id)
wait_for_completion         # websocket_subscribe OR polling_loop
  ↓ (extract image URLs on completion frame)
download_results            # file_output, fan-out if multiple images
  ↓
end (returns local file paths)
```

Branching decision on `wait_for_completion`: inspect traffic during recon; if WS, use `websocket_subscribe`; if polling, use `polling_loop`. Spec supports both; graph picks one.

### `list_models`

Single `http_request` to the model catalog endpoint. Returns available models with their parameter schemas. Used by callers (including Claude via MCP) to pick a valid model and know what params it accepts.

### `history`

```
http_request GET /history?page=N
  ↓ (paginate if needed)
return list of past generations with metadata
```

Enables "regenerate from a past prompt" and "show me what I made last week" without re-querying Freepik's UI.

## MCP exposure

One MCP server hosts all psflow graphs, not one server per service. Tool names are `<service>.<operation>`:

- `freepik.generate(prompt, model, params, reference_image?)`
- `freepik.list_models()`
- `freepik.history(limit, offset)`

Tool param schemas derived from the graph's declared input schema. Responses are the graph's output — for `generate`, local file paths.

## Auth for Freepik specifically

- **Strategy:** `cookie_jar`
- **Source:** persistent cookie store, populated once by manual login in a real browser, exported via a helper script (or HAR capture includes cookies — workable but less clean)
- **Refresh:** none; if cookies expire, re-login manually. Graph execution fails with clear "session expired, re-authenticate" error.

## Synthesis (deferred, not blocking)

When needed:

- **Input:** HAR file from a complete recon browsing session
- **Process:** LLM call with HAR content + target spec format + examples, produces draft `graph.mmd` + schema files
- **Output:** draft that the human (with Claude's help) finalizes

Hand-authored Freepik first, synthesizer later for service #2+.

## What's needed to move forward

Two things to check against psflow's current state:

1. **Which of the five node types above already exist in psflow, which are trivial extensions of existing nodes, and which are genuinely new?** That determines how much work this project actually is versus how much is just "author a graph."

2. **Does psflow's graph context/state model support the kind of inter-node value passing this needs?** Specifically: `submit_generation` produces a `job_id`, which `wait_for_completion` consumes, which produces image URLs that `download_results` consumes. That's standard DAG behavior but the shape of how psflow exposes "this node's output is that node's input" affects how cleanly the graph reads.

Once those two answers are clear, the work partitions cleanly into "extend psflow" (if needed) and "author Freepik graph." The psflow side should be minimal.
