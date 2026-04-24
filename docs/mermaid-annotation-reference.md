# Mermaid Annotation Reference

Reference for `%% @graph` and `%% @node` config knobs. Ctrl-F for the field you need.

---

## 1. Annotation grammar

Every annotation is a `%%` comment. The parser ignores all other lines.

```
%% @<target> <key>: <value>
```

- `<target>` is either the literal string `graph` (for graph-level declarations) or a node ID (the identifier used in the Mermaid topology, e.g. `A`, `Fetch`, `INVOKE`).
- `<key>` is a dot-separated path. Dots create nested JSON objects. `config.url` sets `node.config["url"]`; `config.retry.delay_ms` sets `node.config["retry"]["delay_ms"]`.
- `<value>` is parsed as JSON first. If JSON parsing fails (no quotes, no brackets), the raw text is treated as a string. So `bearer` and `"bearer"` both produce the string `bearer`.

**Value examples:**

| Annotation fragment | Parsed value |
|---|---|
| `config.timeout_ms: 5000` | integer `5000` |
| `config.url: "https://example.com"` | string |
| `config.url: https://example.com` | string (unquoted fallback) |
| `config.fail_on_non_2xx: true` | boolean |
| `config.retry_on: ["5xx", 429]` | JSON array |
| `config.validation.inline: {"type":"object"}` | JSON object |

**Multiline values** use `|` as the raw value and subsequent `%%` continuation lines:

```
%% @A config.script: |
%%   let x = 1;
%%   x + 1
```

**Recognised top-level key prefixes for nodes:**

| Prefix | Target |
|---|---|
| `handler` | `node.handler` (string) |
| `config.<path>` | `node.config` JSON tree |
| `exec.<path>` | `node.exec` JSON tree (execution policy) |
| `inputs.<port>` | declares an input port with the given type name |
| `outputs.<port>` | declares an output port with the given type name |

Unknown top-level keys are stored in `node.config` as a fallback.

---

## 2. Graph-level declarations

All graph-level annotations use `%% @graph <key>: <value>`.

**Metadata fields:**

| Key | Type | Purpose |
|---|---|---|
| `name` | string | Graph display name |
| `version` | string | Version tag |
| `description` | string | Human description |
| `default_executor` | string | Executor hint (not enforced at load time) |
| `required_adapter` | string | AI adapter requirement checked at load |
| `author` | string | Author tag |

### Auth strategy declarations

```
%% @graph auth.<name>.type: <discriminator>
%% @graph auth.<name>.params.<key>: <value>
%% @graph auth.<name>.secrets.<role>: <logical-name>
```

- `<name>` — graph-local strategy name. Nodes reference this name in `config.auth`.
- `<discriminator>` — built-in type string or a host-registered type.
- `params.*` — strategy-specific configuration. Dot-path expansion works: `auth.api.params.scheme: Token` sets `params["scheme"]`. Alternatively set the whole object at once: `auth.api.params: {"scheme":"Token"}`.
- `secrets.<role>` — maps a strategy role name to a logical name the host's `SecretResolver` understands.

**Built-in strategy types:**

#### `static_header`

Injects one fixed header. Good for `X-Api-Key`-style APIs.

| Param | Type | Required | Notes |
|---|---|---|---|
| `params.name` | string | yes | Header name, e.g. `X-Api-Key` |
| `params.value` | string | yes | Header value. Supports `{inputs.x}` / `{ctx.x}` interpolation |

No secrets required by default. If the value needs to come from a resolved secret, use `bearer` or a custom strategy instead of embedding a secret in a template expression.

Supports WS handshake: yes.

```
%% @graph auth.apikey.type: static_header
%% @graph auth.apikey.params.name: X-Api-Key
%% @graph auth.apikey.params.value: abc-123
```

#### `bearer`

Injects `<header>: <scheme> <token>`.

| Param | Type | Default | Notes |
|---|---|---|---|
| `params.header` | string | `Authorization` | Target header name |
| `params.scheme` | string | `Bearer` | Scheme prefix. Set to `""` to inject the raw token |

| Secret role | Required | Maps to |
|---|---|---|
| `token` | yes | The bearer token value |

Supports WS handshake: yes.

```
%% @graph auth.api.type: bearer
%% @graph auth.api.secrets.token: MY_API_TOKEN
```

Custom scheme example:

```
%% @graph auth.internal.type: bearer
%% @graph auth.internal.params.header: X-Auth
%% @graph auth.internal.params.scheme: Token
%% @graph auth.internal.secrets.token: INTERNAL_KEY
```

#### `cookie_jar`

Sends the current per-run jar as a `Cookie:` header; absorbs `Set-Cookie` from each response. The jar lives on `ExecutionContext` and is scoped to the graph run.

| Param | Type | Default | Notes |
|---|---|---|---|
| `params.domain` | string | — | Informational only; no automatic domain filtering |

No required secrets. Pre-seeded sessions are not supported via the params surface.

Supports WS handshake: yes.

```
%% @graph auth.jar.type: cookie_jar
%% @graph auth.jar.params.domain: example.com
```

#### `hmac`

Computes an HMAC signature over a canonical string and injects two headers: the key ID and the hex signature.

Canonical string format (newline-joined):
```
<METHOD>
<PATH_WITH_QUERY>
<signed_headers sorted and formatted as name:value>
<hex(sha256(body))>
```

This is a generic scheme, not byte-compatible with AWS SigV4 or Stripe. Vendor-specific variants need a custom strategy type.

| Param | Type | Default | Notes |
|---|---|---|---|
| `params.algorithm` | `sha256` \| `sha512` | `sha256` | HMAC hash algorithm |
| `params.key_id_header` | string | `X-Key-Id` | Header that carries the key ID |
| `params.signature_header` | string | `X-Signature` | Header that carries the hex signature |
| `params.signed_headers` | string array | `[]` | Header names to include in the canonical string |
| `params.include_body` | bool | `true` | Whether to hash the request body into the canonical string |

| Secret role | Required | Maps to |
|---|---|---|
| `key_id` | yes | Key ID string injected into `key_id_header` |
| `secret` | yes | HMAC signing key bytes |

Supports WS handshake: no (body-dependent signing does not apply to WS upgrades).

```
%% @graph auth.signer.type: hmac
%% @graph auth.signer.params.algorithm: sha256
%% @graph auth.signer.params.key_id_header: X-Key-Id
%% @graph auth.signer.params.signature_header: X-Signature
%% @graph auth.signer.secrets.key_id: SIGNING_KEY_ID
%% @graph auth.signer.secrets.secret: SIGNING_SECRET
```

---

## 3. Handler config surfaces

All node-level config uses `%% @<NodeID> config.<key>: <value>`.

### `http`

Makes an HTTP request. Supports auth, retry, redirect, validation, and body-to-disk streaming.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `url` | string | required | Request URL. Supports `{key}` interpolation |
| `method` | string | `GET` | HTTP method: GET, POST, PUT, PATCH, DELETE, HEAD |
| `headers` | object | `{}` | Map of header name → value template |
| `body` | string | — | Request body template (string). Used for POST/PUT/PATCH |
| `body_json` | bool | `false` | If true, serialize the full inputs map as JSON body; overrides `body` |
| `multipart` | object | — | Multipart/form-data body (see below). Overrides `body` and `body_json` |
| `timeout_ms` | integer | `30000` | Request timeout in milliseconds |
| `allow_private` | bool | `false` | Allow requests to private/loopback IPs (RFC 1918). Default blocks SSRF targets |
| `auth` | string | — | Name of a graph-scoped auth strategy |
| `redirect` | `"none"` \| `"default"` \| `{"max": N}` | `"default"` | Redirect policy. `"default"` follows up to 10 redirects (reqwest default) |
| `retry` | object | — | HTTP-scoped retry policy (see below) |
| `fail_on_non_2xx` | bool | `false` | If true, non-2xx responses fail the node |
| `validation` | object | — | JSON Schema validation of response body (see below) |
| `body_sink` | object | — | Stream response body to disk instead of buffering (see below) |

**`validation` is mutually exclusive with `body_sink`.** A streamed body cannot be validated because it is never buffered. Configuring both is a node validation error.

#### `multipart` shape

```json
{
  "fields": { "field_name": "string-template-value" },
  "files": [
    { "name": "upload", "path": "/tmp/{filename}", "filename": "report.pdf", "content_type": "application/pdf" },
    { "name": "data", "bytes": "raw-string-content", "filename": "data.txt" }
  ]
}
```

File parts accept either `path` (read from disk, template-interpolated) or `bytes` (inline string or u8 array).

#### `redirect` shape

```
%% @N config.redirect: "none"
%% @N config.redirect: "default"
%% @N config.redirect: {"max": 3}
```

#### `retry` shape

```
%% @N config.retry: {
%%   "max_attempts": 3,
%%   "backoff": "exponential",
%%   "delay_ms": 200,
%%   "multiplier": 2.0,
%%   "max_delay_ms": 10000,
%%   "retry_on": ["5xx", "connection_error", 429]
%% }
```

| Retry field | Type | Default | Notes |
|---|---|---|---|
| `max_attempts` | integer | — | Required for retry to activate. Must be `>= 2` |
| `backoff` | `"fixed"` \| `"exponential"` | `"fixed"` | Backoff strategy |
| `delay_ms` | integer | `100` | Initial delay (fixed) or first delay (exponential) |
| `multiplier` | float | `2.0` | Exponential growth factor |
| `max_delay_ms` | integer | `60000` | Cap on exponential delay |
| `retry_on` | array | `["5xx","connection_error"]` | Triggers: status codes, `"5xx"`, `"connection_error"` |

When `retry` is absent or `max_attempts <= 1`, no retry logic runs.

#### `validation` shape

```
%% @N config.validation: { "inline": { "type": "object", "required": ["id"] } }
%% @N config.validation: { "file": "schemas/{schema_name}.json" }
%% @N config.validation.on_failure: passthrough
```

| Validation field | Values | Default | Notes |
|---|---|---|---|
| `inline` | JSON Schema object | — | Schema declared inline |
| `file` | string path template | — | Path to a JSON Schema file. Supports `{key}` interpolation |
| `on_failure` | `"fail"` \| `"passthrough"` | `"fail"` | `fail` fails the node; `passthrough` adds `validation_ok`/`validation_error` to outputs |

`inline` and `file` are mutually exclusive within one `validation` block.

#### `body_sink` shape

```
%% @N config.body_sink: {
%%   "file": {
%%     "path": "/downloads/{filename}",
%%     "overwrite": "if_missing",
%%     "create_parents": true
%%   }
%% }
```

| Sink field | Values | Default | Notes |
|---|---|---|---|
| `file.path` | string path template | required | Destination path. Supports `{key}` interpolation |
| `file.overwrite` | `"always"` \| `"if_missing"` \| `"never"` | `"always"` | `if_missing` skips the network request entirely if the file already exists |
| `file.create_parents` | bool | `true` | Create missing parent directories |

#### `http` outputs

Default (no `body_sink`):

| Field | Type | Notes |
|---|---|---|
| `status` | i64 | HTTP status code |
| `body` | string | Response body |
| `headers` | map | Response headers |
| `validation_ok` | bool | Present when `validation` is configured |
| `validation_error` | map | Present when validation fails in `passthrough` mode |

With `body_sink` and a successful write:

| Field | Type | Notes |
|---|---|---|
| `status` | i64 | |
| `headers` | map | |
| `path` | string | Final on-disk path |
| `bytes_written` | i64 | Bytes streamed to disk |

With `body_sink` and a skipped write (`overwrite: if_missing` or non-2xx in passthrough mode):

| Field | Type | Notes |
|---|---|---|
| `status`, `headers`, `path` | — | As above |
| `bytes_written` | i64 | `0` |
| `skipped` | bool | `true` |
| `error_body_snippet` | string | First 4 KB of error body (non-2xx case only) |

---

### `ws`

Opens a WebSocket connection, optionally sends init frames, reads incoming frames until a termination condition fires, then closes.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `url` | string | required | WebSocket URL. Supports `{key}` interpolation |
| `auth` | string | — | Name of a graph-scoped auth strategy |
| `subprotocol` | string | — | WebSocket subprotocol string sent in the upgrade request |
| `init_frames` | array | `[]` | Frames to send after the connection is established (see below) |
| `terminate` | object | — | Termination criteria (see below) |
| `validation` | object | — | Per-frame JSON Schema validation. Same shape as HTTP `validation` |
| `emit` | `"collect"` \| `{sink_file: {...}}` | `"collect"` | How received frames surface in output (see below) |

**Auth strategies that support WS handshake:** `bearer`, `static_header`, `cookie_jar`. `hmac` does not support WS because it requires a serialised request body.

#### `init_frames` shape

Each entry is either a string (text frame) or an object:

```json
[
  "subscribe to topic",
  { "text": "{\"action\":\"subscribe\",\"channel\":\"{channel}\"}" },
  { "binary": "raw-string-as-bytes" },
  { "binary_bytes": [0, 1, 2] }
]
```

Text frames are template-interpolated from node inputs.

#### `terminate` shape

```
%% @N config.terminate: {
%%   "on_predicate": "frame.done == true",
%%   "max_frames": 100,
%%   "timeout_ms": 30000,
%%   "close_on_terminate": true
%% }
```

| Terminate field | Type | Default | Notes |
|---|---|---|---|
| `on_predicate` | Rhai string | — | Evaluated per received frame. Variables: `frame` (parsed JSON or string), `frame_index` (0-based i64). Return bool |
| `max_frames` | integer | — | Hard frame cap. Terminates after N received frames |
| `timeout_ms` | integer | — | Wall-clock timeout from connection open |
| `close_on_terminate` | bool | `true` | Send a close frame before returning |

Any combination of termination criteria may be active simultaneously; the first to fire wins. External cancellation (cancel token) also terminates the connection.

#### `emit` shape

```
%% @N config.emit: collect

%% @N config.emit: {
%%   "sink_file": {
%%     "path": "/logs/{session}.jsonl",
%%     "overwrite": "always",
%%     "create_parents": true
%%   }
%% }
```

`sink_file` fields are identical to `body_sink.file` in the HTTP handler.

#### `ws` outputs

With `emit: collect`:

| Field | Type | Notes |
|---|---|---|
| `frames` | `Vec<Value>` | Collected frames. Text frames parsed as JSON if valid, otherwise as strings |
| `frame_count` | i64 | Number of received frames |

With `emit: sink_file`:

| Field | Type | Notes |
|---|---|---|
| `path` | string | On-disk path written |
| `frame_count` | i64 | Number of frames written |

Both modes also emit `validation_ok` / `validation_error` when `validation` is configured and `on_failure: passthrough`.

---

### `poll_until`

Invokes a named subgraph on a fixed-delay loop until a Rhai predicate returns true or a max-attempts cap is hit. See also `examples/poll_until_composed.mmd` for the compose-first equivalent pattern.

| Key | Type | Default | Purpose |
|---|---|---|---|
| `graph` | string | required | Name of the subgraph to invoke per attempt |
| `predicate` | Rhai string | required | Evaluated over `output` (subgraph output map) and `attempt` (1-indexed i64). Return bool |
| `max_attempts` | integer | required | Hard cap. Must be `>= 1` |
| `delay_ms` | integer | required | Fixed delay between attempts in milliseconds. First attempt fires immediately |

#### `poll_until` outputs

| Field | Type | Notes |
|---|---|---|
| `attempts_used` | i64 | Number of attempts performed |
| `timed_out` | bool | `true` iff `max_attempts` was reached without the predicate matching |
| `output` | map | Final subgraph output (predicate match or last attempt before cap) |

Cap without a predicate match is not a node failure. Callers branch on `timed_out`.

Example:

```
%% @Poller handler: poll_until
%% @Poller config.graph: check_job_status
%% @Poller config.predicate: output.status == "complete"
%% @Poller config.max_attempts: 20
%% @Poller config.delay_ms: 3000
```

---

### Other handlers (not in the network slice)

These handlers accept `config.*` and `exec.*` annotations like any other node but are not expanded here. See the source files listed for their specific config surfaces.

| Handler | Brief purpose | Source |
|---|---|---|
| `shell` | Run an external command | `src/handlers/shell.rs` |
| `rhai` | Inline or file-backed Rhai script | `src/handlers/rhai_handler.rs` |
| `llm_call` | LLM inference via AI adapter | `src/handlers/llm_call.rs` |
| `subgraph_invoke` | Invoke a named subgraph as a step | `src/handlers/subgraph_invoke.rs` |
| `delay` | Sleep for a fixed duration | `src/handlers/utility.rs` |
| `read_file` / `write_file` / `glob` | File I/O with path templating | `src/handlers/file_io.rs` |
| `accumulator` | Append outputs to a running collection on the blackboard | `src/handlers/accumulator.rs` |

---

## 4. Output shape per handler

Quick reference for threading outputs into downstream predicates and config templates.

| Handler | Output fields |
|---|---|
| `http` (default) | `status` (i64), `body` (string), `headers` (map) |
| `http` + `body_sink` | `status`, `headers`, `path` (string), `bytes_written` (i64), `skipped`? (bool) |
| `http` + `validation` | adds `validation_ok` (bool), `validation_error`? (map) |
| `ws` + `emit: collect` | `frames` (Vec), `frame_count` (i64) |
| `ws` + `emit: sink_file` | `path` (string), `frame_count` (i64) |
| `poll_until` | `attempts_used` (i64), `timed_out` (bool), `output` (map) |

---

## 5. Templating

`{key}` interpolation is available in the following fields. `key` refers to a key in the node's input `Outputs` map.

| Handler | Fields that support `{key}` |
|---|---|
| `http` | `url`, `headers` values, `body`, `body_sink.file.path`, `validation.file` path, `multipart.files[].path` |
| `ws` | `url`, `init_frames` text values |
| `hmac` strategy | `params.key_id_header`, `params.signature_header` (via `AuthApplyCtx::render`) |
| `static_header` strategy | `params.name`, `params.value` |

Template resolution uses psflow's `PromptTemplateResolver` by default. Embedders can supply a custom `TemplateResolver` via `NodeRegistry::with_defaults_and_resolver`.

---

## 6. Predicate expressions (Rhai)

Predicates are small Rhai expressions that return a bool. They are compiled once on first use and cached.

#### WS `terminate.on_predicate`

Scope at evaluation time:

| Variable | Type | Notes |
|---|---|---|
| `frame` | dynamic | The received frame. Text frames parsed as JSON if valid, otherwise a Rhai string |
| `frame_index` | i64 | 0-based index of the current frame |

Return `true` to terminate. The frame that triggers termination is included in the collected output.

```rhai
// Terminate when the server signals done.
frame.done == true

// Terminate after seeing a specific sequence number.
frame_index >= 9
```

#### `poll_until` `predicate`

Scope at evaluation time:

| Variable | Type | Notes |
|---|---|---|
| `output` | map | The subgraph's full output map from the current attempt |
| `attempt` | i64 | 1-based attempt number |

Return `true` to stop polling.

```rhai
// Stop when the job status field is "complete".
output.status == "complete"

// Stop when a numeric field exceeds a threshold.
output.progress >= 100
```
