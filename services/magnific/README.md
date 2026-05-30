# Magnific service (web-API wrapper)

Wraps the **Magnific** (formerly Freepik) web-UI generation API as psflow graphs, exposed over MCP as `magnific.generate`, `magnific.list_models`, and `magnific.history`. Personal, single-user, solo-maintained.

Spec: `ergon/active-documents/20260530-132304-Web-API-Wrapper-Spec.md`.

Status: **live recon captured via Playwright** against an authenticated session. Confirmed endpoints, auth model, completion mechanism, and result-URL pattern are below. The one remaining gap is the exact image-submit path and the WebSocket channel/event names (see §6). Captured payloads live in `fixtures/recon/`.

## 1. Backend shape (confirmed)

Base: `https://www.magnific.com/app/api/`. The browser app is at `/app`; the image generator is `/app/ai-image-generator` (internally "Pikaso").

| Operation | Method + path | Notes |
|---|---|---|
| list_models | `GET /app/api/v2/ai-models?lang=en_US` | Array of ~140 models across tools; `tool="text-to-image"` are the image models (~44: auto, mystic*, flux*, imagen*, seedream*, ideogram, nano-banana, gpt*, recraft, reve, qwen, krea, grok, z-image). Each: `{id, slug, tool, status, inputs{prompt,aspectRatio,numberOfImages,...}, metadata, credits{min,max}}`. |
| history | `GET /app/api/projects/files/recent?page=&per_page=&order_by=created_at` | `{data[], meta}`. Each item: `download_url`, `thumbnail{url,w,h}`, `creation.metadata.prompt`, `created_at`, `tool_name`. |
| cost preview | `POST /app/api/v2/ai/simulate-generation` | Body `{items:[{model,quantity,config:{resolution,numberOfImages}}],forceCredits:true}` → per-item + total credit cost and remaining. Optional pre-flight. |
| generate (submit) | `POST /app/api/{tool}/generate/{mode}` | Observed video variant `POST /app/api/video/generate/auto?mode=auto`. Image submit is analogous; **exact path not yet pinned** (see §6). Body carries the generation config. |
| completion | **WebSocket** (Laravel Echo / Pusher) | Private channel authorized by `POST /app/broadcasting/auth`. Progress + completion are pushed over the socket — **not** polling. |
| result images | `https://pikaso.cdnpk.net/private/production/{id}/render.{jpg|png}?token=exp=...~hmac=...` | Signed, time-limited CDN URLs. Download with psflow `http`+`body_sink`. |

## 2. Auth (confirmed)

Session **cookie** plus an **`x-xsrf-token`** request header (Laravel). The header value is the `XSRF-TOKEN` cookie's value, echoed back per request. State-changing POSTs (generate) require it; the app also sends `x-request-origin` and `x-folder-reference`.

**psflow gap:** the built-in `cookie_jar` strategy sends cookies but does **not** echo a cookie value into a request header. Options to resolve (TODO(author)):
- Extend `cookie_jar` (or add a small strategy) to copy the `XSRF-TOKEN` cookie into the `x-xsrf-token` header, or
- A custom auth strategy that does cookie + CSRF-from-cookie together.

Cookies are seeded once from a manual browser login via the export helper (planned). No refresh — on expiry, generation fails and you re-login.

## 3. Completion is WebSocket, not polling

The earlier spec decision defaulted to polling. **For Magnific the real mechanism is a WebSocket** (Echo/Pusher private channel, authorized by `POST /app/broadcasting/auth`). So `generate` uses `websocket_subscribe` for the wait, terminating on the completion frame, then downloads the result URLs. `poll_until` is not used here (the `check_status` subgraph stub was removed).

## 4. Credits

The recon account is Premium+ with effectively unlimited image generation, so credit spend is not a practical constraint for this wrapper.

## 5. Fixtures captured

- `fixtures/recon/ai-models.json` — full model catalog (list_models response).
- `fixtures/recon/files-recent.json` — history response (`projects/files/recent`).
- `fixtures/recon/simulate-generation-response.json` — cost-preview response.
- `fixtures/recon/auto-mode-constraints-video.json` — the `video/generate/auto` constraints descriptor (captured while pinning the submit pattern; video, not image).

## 6. Remaining recon (one focused pass)

- [ ] Pin the exact **image** submit path + request body by clearing the network log, generating one image, and reading the immediate POST (the buffer overflowed with analytics last time). Likely `POST /app/api/image/generate/{mode}` or `/app/api/v2/ai/...`.
- [ ] Capture the **WebSocket**: the channel name subscribed after `broadcasting/auth`, the event/frame that signals completion, and the field in that frame carrying the result image URLs (vs. having to re-fetch `files/recent`).
- [ ] Confirm whether result URLs arrive in the WS completion frame or must be read from `projects/files/recent` right after.

## 7. Spec corrections captured here

- **Product is Magnific/Pikaso, not Freepik** — rebrand; domain `magnific.com`.
- **Completion is WebSocket**, not polling (§3).
- **Auth needs cookie + `x-xsrf-token`**, which exceeds the built-in `cookie_jar` (§2).
- Auth is graph-level annotations, not an `auth.yaml`.
- `file_output` is `http`+`body_sink` over the signed CDN URL.
- Fan-out over result images is a `parallel-loop:` subgraph.
