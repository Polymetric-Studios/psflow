# Project Terminology

- The project terminology document is the SSOT for all project-related terms and definitions used in the codebase and documents.
- Read the project terminology document (ergon/project-data/project-terminology.md) before starting any work.
- Terms and concepts in documents and code should be well-defined, consistent, and unambiguous.
- There must only be one term for each concept or element.
- Use hyphenated compound naming as needed to disambiguate.
- Refer to and update the project terminology document as needed.

| Term | Definition |
|------|------------|
| AI-adapter | The trait through which the graph accesses LLM intelligence without knowing the backend. |
| ancestor-scoped-filtering | Restricting conversation-history (and trace views) to a node's graph ancestors so parallel branches don't pollute each other. |
| annotated-Mermaid | The single-file definition format: a standard Mermaid `.mmd` file whose node configuration is embedded as structured `%%` comments. Holds topology, typing, node implementation, and execution semantics in one file. |
| annotation | A configuration line in an `.mmd` file. Node form: `%% @<NodeID> <key.path>: <value>`. Graph form: `%% @graph <key>: <value>`. Mermaid renderers ignore these comments. |
| anthropic-API-adapter | AI-adapter backed by the Anthropic API. |
| openai-compatible-adapter | One AI-adapter (`OpenAiCompatAdapter`) for any provider speaking the OpenAI `/v1/chat/completions` wire format with a bearer token (OpenRouter, OpenAI, Groq, Together, local servers). A provider is just a `(base_url, api_key, extra_headers)` triple; the `openrouter` preset points it at `https://openrouter.ai/api`. Stateless — conversational continuity rides on `conversation-history`; arbitrary-model access comes from the per-node `config.model`. |
| auth-strategy | A graph-local, named credential-injection scheme declared via `@graph auth.<name>`. Built-in types: `static_header`, `bearer`, `cookie_jar`, `hmac`. |
| blackboard | Scoped shared state available to nodes and scripts during execution. |
| built-in-handler | A handler shipped with psflow: `passthrough`, `transform`, `delay`, `log`, `merge`, `split`, `gate`, `error_transform`, `http`, `ws`, `poll_until`, `read_file`/`write_file`/`glob`, `rhai`, `llm_call`, `accumulator`, `human_input`, `subgraph_invoke`, `map`, `loop`, `shell`. |
| checkpoint-resume | Saving an execution-snapshot and later resuming: completed nodes skipped, interrupted nodes re-executed; blackboard, branch decisions, and outputs preserved. |
| claude-CLI-adapter | AI-adapter backed by the Claude Code CLI, supporting session strategies. |
| concurrency-limit | A cap on simultaneous execution, applied globally, per-parallel, or per-adapter. |
| config | A node's `config` JSON tree, set via `config.<path>` annotations; the handler's parameters. |
| conversation-history | The `ConversationHistory` accumulated on the blackboard from LLM prompt/response exchanges, fed to subsequent LLM nodes. |
| cookie-jar | Per-run cookie store on the execution-context; sends `Cookie:` and absorbs `Set-Cookie`. Backs the `cookie_jar` auth-strategy. |
| cross-run-state | Per-graph state persisted across runs by **psflow-run**: node outputs prefixed `save_` are written to a state file and reloaded as `{ctx.*}` on the next run (precedence config < state < **runtime-input**). |
| default-executor | Graph-level executor hint (`@graph default_executor`); not enforced at load time. |
| edge | A directed connection between two nodes; carries typed tokens during execution. |
| event-driven-executor | Execution where external events push into entry nodes. |
| exec | A node's `exec` JSON tree (execution policy) — concurrency, timeout, retry, and similar runtime controls. |
| execution-context | Runtime state for a graph run: blackboard, cancellation, concurrency limits, and the per-run cookie jar. |
| execution-event-bus | The `tokio::broadcast` channel emitting execution events for observability. |
| execution-snapshot | The serialized full execution state (`ExecutionSnapshot`), saved as JSON. |
| execution-trace | The record of a run: per-node timing, outputs, and retry history. Supports live mid-execution and ancestor-scoped views. |
| executor | A swappable strategy that walks the graph. Selected at runtime via trait objects. |
| graph | The immutable topology: nodes, typed ports, directed edges, and nested subgraphs. Backed by petgraph. |
| graph-library | The registry of named **subgraph**s — every `.mmd` loaded by file stem — that `subgraph_invoke`, **map**, and **loop** invoke by name. |
| guard | A predicate expression (Rhai) controlling conditional flow in `branch`, `gate`, `loop`, etc. |
| handler | A node's implementation — the code that executes when the node runs. Built-in, Rhai, or custom. |
| integration-handler | A third-party **handler** registered by **psflow-run** (in `register_integrations`) rather than the provider-neutral engine, kept isolated so it can be removed without touching the core. |
| listen-mode | **psflow-run** `--listen`: reads JSON events from the `PSFLOW_LISTEN_CMD` command and runs a handler **named-graph** per event with the event as `{ctx.event}`; the provider-neutral trigger bridge. |
| LLM-oracle-node | A node that delegates a branch/race/loop decision to an LLM. |
| logical-name | The host-understood secret identifier that a secret-role maps to. |
| loop | A **handler** that loops a **subgraph**, accumulating its produced items (optionally deduped) until an `until` predicate, N dry rounds, or an iteration cap; generalizes **poll-until**. |
| loop-controller | The component managing iteration for loop nodes. |
| map | A **handler** that fans a **subgraph** out over a runtime list — one invocation per element, concurrency-capped and order-preserved — then reduces results (`collect` or `quorum`). |
| mock-adapter | Deterministic AI-adapter for testing. |
| named-graph | A `.mmd` in the graphs directory addressed by its file stem; **psflow-run** runs them and the **graph-library** invokes them by this name. |
| node | A unit of work in the graph. Carries a handler, config, exec policy, and declared ports. |
| node-ID | The Mermaid identifier for a node (e.g. `A`), used as the annotation target `@A`. |
| on-failure-hook | An optional `on-failure` **named-graph** that **psflow-run** runs (passing the error as input), plus a desktop notification, when a run has a failed node. |
| poll-until | A **handler** that loops a **subgraph** on a fixed-delay attempt cap until a Rhai predicate over its output is satisfied; the fixed-attempt case generalized by **loop**. |
| port | A typed connection point on a node. An input-port consumes a value; an output-port produces one. Types include string, bool, i64, f32, Vec, Map, and domain types. |
| psflow | The domain-agnostic graph execution engine. A universal graph data model with swappable executor strategies. |
| psflow-run | The personal runner binary that executes **named-graph**s with **runtime-input**s and wires `llm_call`, adding **cross-run-state**, scheduling, **tool-response-cache**, **run-record**s, **listen-mode**, and **on-failure-hook**; provider-neutral, with **integration-handler**s registered separately. |
| reactive-executor | Fire-on-input-ready execution that propagates downstream (also called dataflow). |
| required-adapter | Graph-level AI-adapter requirement (`@graph required_adapter`) checked at load. |
| Rhai | The sandboxed scripting engine (with execution limits) for guards and the `rhai` handler. Scripts are inline or external `.rhai` files. |
| run-record | A per-run JSON file **psflow-run** writes to the runs directory: status, node states, the **execution-trace**, and any tool `log_id`s. |
| runtime-input | A `--input key=value` given to **psflow-run**, exposed to handler **templating** as `{ctx.key}` (layered config < **cross-run-state** < input). |
| secret-resolver | The host-implemented `SecretResolver` that maps a logical-name to an actual secret value. |
| secret-role | A strategy-defined role name (e.g. `token`) mapped to a logical-name via `auth.<name>.secrets.<role>`. |
| session-strategy | The Claude CLI adapter's session-reuse policy: `new`, `continue`, `named`, or `pool`. |
| stepped-executor | One evaluation cycle per call, behavior-tree style (also called tick). |
| subgraph | A nested/hierarchical graph embedded as a node, invoked via the `subgraph_invoke` handler. |
| templating | Interpolation of `{inputs.x}` / `{ctx.x}` expressions into config string values (e.g. URLs, paths, headers). |
| token | A typed value carried on an edge between nodes. |
| tool-response-cache | An opt-in cache of tool-**handler** responses keyed by (tool, arguments), enabled via `PSFLOW_TOOL_CACHE_*`; supports offline record/replay and TTL modes. |
| topological-executor | Dependency-ordered parallel execution in waves via tokio (also called batch). |
| WS-handshake | Auth applied to a WebSocket connection upgrade, supported by select auth-strategies. |
