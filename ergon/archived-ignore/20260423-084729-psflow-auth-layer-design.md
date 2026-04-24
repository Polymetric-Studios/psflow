# 20260423-084729-psflow-auth-layer-design.md

Design note: graph-scoped auth strategy layer for psflow (workstream 1).

Companion to the network/auth audit captured in `20260423-084034-psflow-network-and-auth-update.md`. This note pins the trait shapes and integration seams. Implementation is deferred; no code here.

---

## 1. Context

Every network-adjacent handler today (HTTP, and the WS handler coming in this update) constructs requests inline from `node.config`. Authentication, when needed, leaks into each node as hand-written `headers: { "Authorization": "Bearer {token}" }` entries with the token shipped through a template variable — meaning credentials either sit in the graph source, travel through the blackboard, or arrive as a node input. All three are wrong: they conflate credential resolution with request shape, they fan out the same secret across every node that calls the same API, and they give psflow no structured place to reason about auth.

The auth layer fixes this by declaring **named auth strategies at graph scope**. A strategy is a reusable, typed description of *how* to authenticate — bearer token, HMAC signature, cookie jar, etc. — that binds to logical secret keys. Network handlers reference a strategy by name in their config; at request-build time the runtime resolves the strategy and applies it to the outgoing request. Secrets themselves come from a host-provided resolver. psflow never sees storage, rotation, or lifecycle — it sees already-resolved opaque values at the last possible moment.

Explicitly out of scope: secret storage, vault integration, rotation, credential audit trails, TLS client cert management (that's a deeper `reqwest::Client` concern and belongs in the transport layer, not the auth layer).

## 2. Shape of the strategy trait

The strategy is the abstraction that turns "I need to authenticate this request" into a concrete mutation. Three shape candidates, picked and justified.

### 2.1 Apply semantics: mutate the builder

The strategy receives the partially-built `reqwest::RequestBuilder` and returns a (possibly mutated) builder. Rejected alternatives: returning header/cookie deltas forces us to re-enumerate every mutation type any future strategy could want (body rewrite, query params, client-cert selection); returning an opaque apply-closure hides the seam and makes introspection impossible. Mutating the builder is what handlers already do — headers, body, timeouts — so strategies slot into the same seam the handler already uses. Conceptually:

```
async fn apply(
    &self,
    ctx: &AuthApplyCtx<'_>,
    builder: RequestBuilder,
) -> Result<RequestBuilder, AuthError>
```

`AuthApplyCtx` bundles the resolved secret values, a reference to the execution context (for blackboard reads during template interpolation of strategy params), and a view of the request metadata the strategy may need to read without consuming the builder (method, URL, body bytes for signing — see 2.3).

### 2.2 Async, fallible

Yes on both. HMAC can be synchronous but the secret resolution that precedes it is async, and the trait must accommodate the slowest legitimate strategy — future OAuth token-exchange or signed-URL strategies will want outbound I/O. One async method is simpler than two trait layers ("resolve-then-apply"). Fallibility is required for secret-missing, malformed-param, and signing-failed cases. Error type is a dedicated `AuthError` distinct from `NodeError` so handlers can decide whether to wrap it as recoverable.

### 2.3 Handling body-dependent strategies

HMAC-signed requests need the final serialized body to compute the signature. The current HTTP handler builds the body last and calls `.body(...)` on the builder. The cleanest seam: `apply` runs **after** the handler has set the body and before `.send()`. For strategies that don't care about the body (static header, bearer) this is a no-op concern. For HMAC the strategy reads the body off the builder — but `reqwest::RequestBuilder` does not expose the set body. So either (a) the handler passes the serialized body bytes alongside the builder in `AuthApplyCtx`, or (b) the handler clones the builder's request via `try_clone()` and inspects it. Option (a) is explicit and cheaper; take it.

### 2.4 Enum vs. trait object

Trait object wins, narrowly. The four built-ins could live as an enum and would compile faster, but the constraint "strategy types are pluggable" means hosts register their own — most plausibly an OAuth2 strategy, a project-specific signed-envelope strategy, or a test-only no-op. An enum forces every host to either fork psflow or reach into the `Custom(Box<dyn Strategy>)` escape hatch, at which point the enum is doing nothing for us. Built-ins ship as individual `impl AuthStrategy` types and register into an `AuthStrategyRegistry` keyed by *type discriminator* (e.g. `"bearer"`, `"hmac"`). The registry maps discriminator → factory, where the factory takes the per-strategy params map and produces an `Arc<dyn AuthStrategy>`. This mirrors how `NodeRegistry` handles handlers today and keeps the extension seam symmetric.

The tension: trait objects mean async-trait overhead and `Box<dyn Future>` returns in the apply hot path. That's acceptable — auth application happens once per network node execution, not per message, and the HTTP handler is already `Pin<Box<dyn Future>>`.

## 3. Shape of the secret resolver trait

Host-provided. psflow declares the trait, hosts implement it, and the resolver is passed in at `NodeRegistry` or `ExecutionContext` construction time.

### 3.1 Input: structured key

A bare string key ("anthropic_api_key") is tempting but too lossy. Strategies need to distinguish "the signing key for strategy `foo`" from "the signing key for strategy `bar`" even when both are HMAC and both bind to keys the host calls "signing_key". The input is a small struct: `{ strategy_name: String, logical_name: String }`. The host can ignore `strategy_name` if its key space is flat, or use it as a namespace. Reason to prefer this over a single concatenated string: it preserves the host's ability to apply per-strategy policy (different vault paths, different audit tags) without string-parsing.

### 3.2 Output: zeroizable opaque value

Return type is `SecretValue` — a newtype around `Box<[u8]>` (or `Zeroizing<Vec<u8>>` if we pull in the `zeroize` crate, which we should) with `Debug` implemented to print `SecretValue(***)` and no public `Display`. Strategies that need string-typed secrets (bearer tokens) get a `.as_str()` that returns `&str` but only through a method explicitly named to discourage casual logging. No `Serialize`. Accidental inclusion in a JSON log becomes a compile error.

### 3.3 Async, fallible, caching

Async yes (vaults are remote). Fallible yes (a missing binding is a real error, not a panic). Caching is the host's concern, not psflow's — the trait contract is "given this key, give me the current value." Hosts that want per-run caching wrap their own resolver. psflow calls the resolver at least once per strategy application; it does not memoize, because memoization interacts with rotation semantics we explicitly don't want to own.

```
async fn resolve(
    &self,
    request: &SecretRequest,
) -> Result<SecretValue, SecretError>
```

### 3.4 How strategies declare needed keys

Each strategy declares its bindings statically in its params. Concretely, the per-strategy params include a `secrets` sub-object that maps *logical role names the strategy cares about* (e.g. `token` for bearer, `key_id` and `secret` for HMAC) to *logical names the resolver understands* (e.g. `token` → `"anthropic_api_key"`). The strategy reads from the first; the resolver is queried with the second. This indirection means the same bearer strategy type can be instantiated twice under different graph-level names, each pointing at a different host-side secret, without any strategy-internal branching.

A load-time validation pass can ask each strategy "what role names do you require?" and check that the params cover them — surface the error at graph load, not mid-execution.

## 4. Graph-level declaration

### 4.1 Where it lives

`GraphMetadata` already exists as the graph-scoped config home. Add a typed field rather than shoving strategies into `extras`:

```
pub struct GraphMetadata {
    ...existing fields...
    pub auth: BTreeMap<String, AuthStrategyDecl>,
}
```

Stored as a map keyed by the strategy's graph-local name (the name handlers reference). `AuthStrategyDecl` carries:

- `type_: String` — discriminator (`"bearer"`, `"hmac"`, `"static_header"`, `"cookie_jar"`, or a host-registered name)
- `params: serde_json::Value` — an object whose shape the strategy's factory validates
- `secrets: BTreeMap<String, String>` — role-to-logical-name mapping (section 3.4)

### 4.2 Mermaid surface

Annotation form is `%% @auth <name> type: "bearer" params: { ... } secrets: { token: "my_api_key" }`, parsed into `GraphMetadata.auth`. The parsing rule is an extension of the existing `%% @graph` annotation handling; no new parser plumbing beyond a new annotation prefix.

### 4.3 Params interpolation

Params may contain template strings — e.g. the HMAC `key_id` might be `"{ctx.env}-signer"`. The strategy factory does **not** interpolate at load time because `ExecutionContext` isn't available yet. Interpolation happens inside `apply()` using the existing `TemplateResolver` held by the registry. This means strategy factories receive the raw param JSON and either pre-compile template positions (optimization) or re-render on each apply (simpler — do this first).

## 5. Handler integration

### 5.1 Config surface

HTTP handler gains one config key: `auth: "<strategy-name>"`. The value is a flat string referring to an entry in `GraphMetadata.auth`. No inline strategy definition — forcing declaration at graph scope is the whole point.

### 5.2 Lookup timing

Two valid places:

1. **Load time** — resolve name → strategy instance when the graph is loaded, attach the resolved `Arc<dyn AuthStrategy>` to the node. Fast at execution time; fails early on typos.
2. **Execution time** — look up by name when the node runs.

Pick (1) for name resolution and unknown-strategy errors; pick (2) for the actual secret-resolver call and strategy `apply`. That is: `AuthStrategyRegistry` is built once from `GraphMetadata.auth` at graph load, and the HTTP handler receives the registry through its existing construction path (analogous to how `ShellHandler` receives the template resolver). At node execution, the handler reads `config.auth`, looks up the strategy, calls `apply(&builder)`.

### 5.3 Validation

At graph load:

- `auth` entries whose `type_` is not a registered strategy factory → load error.
- Strategy `required_roles()` not covered by `secrets` map → load error.
- Node `config.auth` pointing at an undeclared strategy name → load error.

At execution:

- Secret resolver returning an error → node fails with a recoverable `AuthError::SecretResolution` (retry may succeed if the resolver is talking to a transient backend).
- Strategy `apply` internal error → node fails, non-recoverable.

## 6. The four built-ins

### 6.1 Static header

*Params*: `{ name: string, value: string }`. *Secrets*: none by default, but `value` may template-interpolate from resolved secrets via a `{secrets.foo}` placeholder resolved against the strategy's `secrets` map. *Injects*: one header. Trivial. Use case: `X-Api-Key` style APIs where "auth" is one fixed header.

### 6.2 Bearer token

*Params*: `{ scheme: string = "Bearer", header: string = "Authorization" }`. *Secrets*: role `token`. *Injects*: `<header>: <scheme> <token>`. Also trivial, but common enough to deserve its own type rather than asking everyone to configure static-header with templated value.

### 6.3 Cookie jar

*Params*: `{ domain: string, scope: "run" | "strategy" }`. *Secrets*: optional — for pre-seeded sessions. *Injects*: `Cookie:` header from current jar state. *Hard parts*: state. A cookie jar is not a pure function of secrets; it mutates as responses set cookies. Two decisions follow:

- Jar storage lives on `ExecutionContext` (a new `AuthState` field alongside the blackboard) keyed by `(strategy_name, scope)`.
- Scope `run` means "one jar per graph run, shared across all nodes using this strategy name" — the typical case. Scope `strategy` means "one jar per strategy, long-lived across runs if the host reuses context" — useful for session-based scrapers but carries leak risk and needs host opt-in.
- After a response comes back, the strategy needs a **post-apply hook** to capture `Set-Cookie`. This adds a second method to the trait: `async fn observe_response(&self, ctx, response_headers) -> Result<(), AuthError>`. Default impl is a no-op; cookie jar overrides it. The HTTP handler calls it between receiving the response and extracting the body. Flag: this is the one place the trait grows beyond pure request mutation. Worth the cost; without it the cookie jar is just a bearer token with extra steps.

### 6.4 HMAC-signed request

*Params*: `{ algorithm: "sha256" | "sha512", key_id_header: string, signature_header: string, signed_headers: [string], include_body: bool }`. *Secrets*: roles `key_id`, `secret`. *Injects*: `key_id_header: <key_id>` and `signature_header: <hex signature>`. *Hard parts*: needs the final body bytes (see 2.3) and the canonical request string convention — every HMAC-signing API (AWS SigV4, Stripe, legacy custom ones) has a slightly different canonicalization. The shipped built-in implements a **generic HMAC over a canonical string** (method + path + sorted signed headers + body hash) that covers most custom APIs. AWS SigV4 specifically will want its own strategy; we don't ship it but the seam supports it.

## 7. Integration order

Auth layer is workstream 1 because:

- The header injection hook that section 6 of the audit proposal calls out — "HTTP handler should have a point where external code can inject headers before send" — **is** the strategy `apply` hook. There is no separate header-injection extension point; there's an auth-apply extension point, and static-header strategy is the header injection use case. Building both is duplication.
- WS handler work (also in the update) needs the same hook. If the auth trait lands first, WS can slot in the same `apply` call at connect time (with minor shape differences — WS auth runs during the upgrade handshake, not per-message).
- Cookie jar drives the addition of `AuthState` on `ExecutionContext`, which in turn affects snapshot/resume semantics. Doing this once, during workstream 1, means the snapshot format only churns once.

Order within the workstream:

1. Trait definitions + `SecretResolver` + `AuthError` + `SecretValue`.
2. `AuthStrategyRegistry` + graph metadata surface + Mermaid annotation parsing.
3. HTTP handler integration (the `apply` call site).
4. Static header and bearer built-ins (minimum viable, unblocks most real APIs).
5. HMAC built-in.
6. Cookie jar (requires `AuthState` on context + `observe_response` hook).
7. WS handler integration (workstream 2 picks this up, but the trait is already stable).

## 8. Decisions locked

1. **Secret value wrapper**: use the `zeroize` crate. Standard, audited, interops with the wider ecosystem.
2. **Cookie jar default scope**: per graph run. Jar lifetime matches `ExecutionContext`; no cross-run bleed.
3. **Strategy chaining**: exactly one strategy per node. If layered auth (e.g. bearer + HMAC) is later needed, add a `Composite` built-in rather than growing the node config surface.
4. **Node-level override**: strict reference-by-name. Graph-level declarations are SSOT; nodes cannot inline-overlay params.
5. **Resolver timing**: lazy at node execution. Load-time validation is limited to shape checks (declared strategies exist; all role names bound). No graph-load resolver dry-run.
6. **`apply` signature**: concrete `reqwest::RequestBuilder`. WS integration, when it lands, gets its own surface on the same trait rather than paying upfront abstraction cost.
