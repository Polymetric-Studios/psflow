# Magnific service (web-API wrapper)

Wraps the **Magnific** (formerly Freepik) web-UI generation API as psflow graphs, exposed over MCP as `magnific.generate`, `magnific.list_models`, and `magnific.history`. Personal, single-user, solo-maintained.

Spec: `ergon/active-documents/20260530-132304-Web-API-Wrapper-Spec.md`.

Status: **recon essentially complete** — captured via Playwright (network interceptor) against an authenticated session. The full generate flow, endpoints, auth model, WebSocket channel, and result-URL linkage are confirmed below. Only the exact WS completion *event name/payload* is still unconfirmed (§6). Captured payloads live in `fixtures/recon/`.

## 1. Backend shape (confirmed)

Base: `https://www.magnific.com/app/api/`. The browser app is at `/app`; the image generator is `/app/ai-image-generator` (internally "Pikaso").

| Operation | Method + path | Notes |
|---|---|---|
| list_models | `GET /app/api/v2/ai-models?lang=en_US` | Array of ~140 models across tools; `tool="text-to-image"` are the image models (~44: auto, mystic*, flux*, imagen*, seedream*, ideogram, nano-banana, gpt*, recraft, reve, qwen, krea, grok, z-image). Each: `{id, slug, tool, status, inputs{prompt,aspectRatio,numberOfImages,...}, metadata, credits{min,max}}`. |
| history | `GET /app/api/projects/files/recent?page=&per_page=&order_by=created_at` | `{data[], meta}`. Each item: `download_url`, `thumbnail{url,w,h}`, `creation.metadata.prompt`, `created_at`, `tool_name`. |
| cost preview | `POST /app/api/v2/ai/simulate-generation` | Body `{items:[{model,quantity,config:{resolution,numberOfImages}}],forceCredits:true}` → per-item + total credit cost and remaining. Optional pre-flight. |
| generate — start | `POST /app/api/start-tti-v2` | Body `{mode:<model-slug>, prompt, references:[], num_images, aspect_ratio, variations, force_credits:true}` → `{family:<uuid>, request_tokens:[...], available_slots, limit}`. Reserves a generation family + a token per image. |
| generate — render (×N) | `POST https://ak-data.magnific.com/app/api/render/v4` | One call per image. Body `{tool:"text-to-image", mode, family, prompt, width, height, seed, aspect_ratio, resolution, request_token, image_index, num_images, ...}` → `{creation:{id, identifier, family, metadata{seed, expectTime, creditLedger,...}}}`. **`creation.id` keys the result URL.** Note the separate host `ak-data.magnific.com`. |
| completion | **WebSocket (self-hosted Pusher, protocol 7)** | `wss://ak-data.magnific.com/app/app/xzo0bvj9t7raco6og0q3?protocol=7&client=js&version=8.4.0`. Per-user private channel **`private-user.{id}`**, authorized by `POST /app/broadcasting/auth` (req body `socket_id=…&channel_name=private-user.{id}`; resp `{auth:"<key>:<hmac>"}`). Handshake: connect → `pusher:connection_established` (`data.socket_id`) → broadcasting/auth → `pusher:subscribe`. Completion pushed per `creation` — **not** polling. |
| result images | `https://pikaso.cdnpk.net/private/production/{creation.id}/render.{jpg|png}?token=exp=...~hmac=...` | Signed, time-limited CDN URLs, keyed by `creation.id` from the render call. Download with psflow `http`+`body_sink`. |

## 2. Auth (confirmed)

Session **cookie** plus an **`x-xsrf-token`** request header (Laravel). The header value is the `XSRF-TOKEN` cookie's value, echoed back per request. State-changing POSTs (generate) require it; the app also sends `x-request-origin` and `x-folder-reference`.

**psflow gap #1 — RESOLVED.** `cookie_jar` now echoes a cookie into a request header via `params.csrf_cookie` + `params.csrf_header` (URL-decoded by default). The graphs declare `csrf_cookie: XSRF-TOKEN`, `csrf_header: x-xsrf-token`. See the annotation reference's `cookie_jar` section.

Cookies are seeded once from a manual browser login via the export helper (planned). No refresh — on expiry, generation fails and you re-login.

## 3. Completion is WebSocket, not polling

The earlier spec decision defaulted to polling. **For Magnific the real mechanism is a Pusher WebSocket.** The client subscribes to the per-user channel `private-user.{id}` (authorized via `POST /app/broadcasting/auth`); completion is pushed per `creation`. So `generate` is: `start-tti-v2` → fan-out `render/v4` per image (each yields a `creation.id`) → `websocket_subscribe` waiting for the completion event(s) matching those `creation.id`s → download each signed `pikaso.cdnpk.net/.../{creation.id}/render.{ext}`. `poll_until` is not used (the `check_status` subgraph stub was removed). The channel is per-user, not per-job, so the graph must correlate WS events to its own `creation.id`s.

## 4. Credits

The recon account is Premium+ with effectively unlimited image generation, so credit spend is not a practical constraint for this wrapper.

## 5. Fixtures captured

- `fixtures/recon/ai-models.json` — full model catalog (list_models response).
- `fixtures/recon/files-recent.json` — history response (`projects/files/recent`).
- `fixtures/recon/simulate-generation-response.json` — cost-preview response.
- `fixtures/recon/start-tti-v2-response.json` — generate-start response (`family`, `request_tokens`).
- `fixtures/recon/render-v4-response.json` — per-image render response (`creation.id`, metadata).
- `fixtures/recon/auto-mode-constraints-video.json` — the `video/generate/auto` constraints descriptor (captured while pinning the submit pattern; video, not image).

## 6. psflow gaps + remaining work

**Gap #1 (cookie → header CSRF echo) — DONE.** `cookie_jar` now supports `csrf_cookie`/`csrf_header` (§2).

**Gap #2 (reactive WS handshake) — NOT YET IMPLEMENTED.** The Pusher private-channel subscribe is a *reactive* handshake the `ws` handler can't express today: connect → read `socket_id` from the `pusher:connection_established` frame → mid-stream HTTP `POST /app/broadcasting/auth` (cookie+xsrf) → send a computed `pusher:subscribe` frame → then receive channel events. The handler currently only sends **static** `init_frames`. Implementing this needs a new `config.handshake`-style capability on the `ws` handler (receive-frame → extract → auth-subcall → send-frame), plus a mock-Pusher test harness (no live-WS test harness exists yet). The endpoint + full handshake are now confirmed (§1, §3), so this is a well-scoped build.

- [ ] Implement the reactive WS handshake capability on the `ws` handler (Gap #2).
- [ ] Capture the exact **completion event** name/payload on `private-user.{id}` (whether the signed URL is in the frame or constructed from `creation.id`). Needs a WS-frame interceptor installed before the socket opens; deferrable since the URL is derivable from `creation.id`.
- [ ] Author the `render/v4` fan-out (`parallel-loop` over `request_tokens`) and the WS-event↔`creation.id` correlation in `generate.mmd`.

## 7. Spec corrections captured here

- **Product is Magnific/Pikaso, not Freepik** — rebrand; domain `magnific.com`.
- **Completion is WebSocket**, not polling (§3).
- **Auth needs cookie + `x-xsrf-token`**, which exceeds the built-in `cookie_jar` (§2).
- Auth is graph-level annotations, not an `auth.yaml`.
- `file_output` is `http`+`body_sink` over the signed CDN URL.
- Fan-out over result images is a `parallel-loop:` subgraph.
