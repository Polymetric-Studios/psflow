# Freepik service (web-API wrapper)

Wraps Freepik's undocumented web-UI generation API as psflow graphs, exposed over MCP as `freepik.generate`, `freepik.list_models`, and `freepik.history`. Personal, single-user, solo-maintained.

Spec: `ergon/active-documents/20260530-132304-Web-API-Wrapper-Spec.md`.

Status: **scaffold + recon pending.** The `.mmd` graphs and `schemas/` are skeletons. Every value that depends on observing real traffic is marked `TODO(recon)`; every wiring decision left for authoring is marked `TODO(author)`. Nothing here runs until the recon pass (§4) fills those in.

## 1. Layout

- `generate.mmd` — submit → wait (poll) → fan-out download. The main operation.
- `check_status.mmd` — the named subgraph `poll_until` invokes each attempt; returns the current job state.
- `list_models.mmd` — single request to the model catalog.
- `history.mmd` — paginated request for past generations.
- `schemas/` — JSON Schema files referenced by `http` nodes' `config.validation.file`.
- `fixtures/` — captured example payloads, kept for later drift checks.

## 2. How it runs

1. Auth is a **graph-level annotation**, not a file: each graph declares `%% @graph auth.freepik.type: cookie_jar`. The cookie jar lives on the execution context for the run.
2. `http` nodes name the strategy via `config.auth: freepik`; the jar injects the `Cookie:` header and absorbs `Set-Cookie` on each response, scoped to the `freepik.com` domain.
3. MCP tool parameter schemas are auto-derived from each graph's declared input ports (`inputs.*`); descriptions are hand-written so the LLM caller knows what `prompt`/`model`/`params` mean.

## 3. One-time setup (manual login + cookie export)

The jar has no refresh. When cookies expire, generation fails with a clear "re-authenticate" condition and you repeat this.

- [ ] Log into Freepik in a real browser.
- [ ] Run the cookie-export helper to capture the session cookies into the persisted jar file the graph loads at run start. (Helper is a planned task — see the spec §9.1.)
- [ ] Confirm the exported cookie set is scoped to `freepik.com`.

## 4. Recon checklist

Capture a HAR via browser DevTools while driving the real Freepik UI, then fill every `TODO(recon)` from it. Capture targets:

- [ ] **Generate submit** — method, URL, request headers, request body shape, and the response field that carries the job identifier.
- [ ] **Completion mechanism** — confirm whether progress arrives via repeated status GETs (polling — the chosen default) or a WebSocket. If WS, note the upgrade URL and the completion frame shape (the spec keeps WS as a deferred fallback).
- [ ] **Status poll** — the status endpoint, the field that signals "complete", and the field(s) carrying the result image URLs.
- [ ] **Result URLs** — whether they are signed/time-limited, and the filename or id to template into the local output path.
- [ ] **Model catalog** — the `list_models` endpoint and its response shape (model id + accepted params per model).
- [ ] **History** — the history endpoint, its pagination scheme (page/offset/cursor), and the per-item metadata shape.
- [ ] **Auth surface** — which cookies are actually required, and whether any non-cookie header (CSRF token, signature) is sent. If a signed header appears, revisit the auth strategy choice.
- [ ] Save representative request/response pairs into `fixtures/`.

## 5. Open authoring notes (resolve after recon)

- [ ] **job_id extraction.** `http` emits the raw `body` string; add a `rhai` node after submit to parse the JSON and emit a typed `job_id`. Field path is `TODO(recon)`.
- [ ] **URL-list publication for fan-out.** The `parallel-loop:` download reads its collection from a blackboard key via `exec.loop_foreach`. Decide how the parsed URL `Vec` is published to that key (promotion vs an explicit writer node), then wire it. `TODO(author)`.
- [ ] **Per-item download path.** Inside the loop, the URL is exposed as `loop.item`; the download node reads `{loop.item}` for `config.url` and templates the local `body_sink.file.path`.
- [ ] **`poll_until` + subgraph registration.** `poll_until` is a real handler but is **not** in the bare `psflow` CLI's default registry (it needs the subgraph library wired in) — the standalone `--validate` warns it is unregistered, which is expected. The MCP host must register the `poll_until` handler and register `check_status` into the graph library so `generate.mmd`'s `config.graph: check_status` resolves.

## 6. Spec corrections captured here

- **Auth is graph annotations, not `auth.yaml`.** The original spec proposed an `auth.yaml`; psflow declares auth via `%% @graph auth.*`. There is no `auth.yaml`.
- **`file_output` is `http` + `body_sink`.** Downloading a (possibly signed) URL to disk is the `http` node with the signed URL as `config.url` and a `body_sink.file` destination — not a separate node type.
- **Fan-out is `parallel-loop:`.** Multiple result images are downloaded by a `parallel-loop:` subgraph over the URL list, not an in-node feature.
