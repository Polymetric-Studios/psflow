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
| auth-strategy | A graph-local, named credential-injection scheme declared via `@graph auth.<name>`. Built-in types: `static_header`, `bearer`, `cookie_jar`, `hmac`. |
| blackboard | Scoped shared state available to nodes and scripts during execution. |
| built-in-handler | A handler shipped with psflow: `passthrough`, `transform`, `delay`, `log`, `merge`, `split`, `gate`, `error_transform`, `http`, `ws`, `poll_until`, `read_file`/`write_file`/`glob`, `rhai`, `llm_call`, `accumulator`, `human_input`, `subgraph_invoke`, `shell`. |
| checkpoint-resume | Saving an execution-snapshot and later resuming: completed nodes skipped, interrupted nodes re-executed; blackboard, branch decisions, and outputs preserved. |
| claude-CLI-adapter | AI-adapter backed by the Claude Code CLI, supporting session strategies. |
| concurrency-limit | A cap on simultaneous execution, applied globally, per-parallel, or per-adapter. |
| config | A node's `config` JSON tree, set via `config.<path>` annotations; the handler's parameters. |
| conversation-history | The `ConversationHistory` accumulated on the blackboard from LLM prompt/response exchanges, fed to subsequent LLM nodes. |
| cookie-jar | Per-run cookie store on the execution-context; sends `Cookie:` and absorbs `Set-Cookie`. Backs the `cookie_jar` auth-strategy. |
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
| guard | A predicate expression (Rhai) controlling conditional flow in `branch`, `gate`, `loop`, etc. |
| handler | A node's implementation — the code that executes when the node runs. Built-in, Rhai, or custom. |
| LLM-oracle-node | A node that delegates a branch/race/loop decision to an LLM. |
| logical-name | The host-understood secret identifier that a secret-role maps to. |
| loop-controller | The component managing iteration for loop nodes. |
| mock-adapter | Deterministic AI-adapter for testing. |
| node | A unit of work in the graph. Carries a handler, config, exec policy, and declared ports. |
| node-ID | The Mermaid identifier for a node (e.g. `A`), used as the annotation target `@A`. |
| poll-until | A handler that repeatedly requests until a predicate is satisfied. |
| port | A typed connection point on a node. An input-port consumes a value; an output-port produces one. Types include string, bool, i64, f32, Vec, Map, and domain types. |
| psflow | The domain-agnostic graph execution engine. A universal graph data model with swappable executor strategies. |
| reactive-executor | Fire-on-input-ready execution that propagates downstream (also called dataflow). |
| required-adapter | Graph-level AI-adapter requirement (`@graph required_adapter`) checked at load. |
| Rhai | The sandboxed scripting engine (with execution limits) for guards and the `rhai` handler. Scripts are inline or external `.rhai` files. |
| secret-resolver | The host-implemented `SecretResolver` that maps a logical-name to an actual secret value. |
| secret-role | A strategy-defined role name (e.g. `token`) mapped to a logical-name via `auth.<name>.secrets.<role>`. |
| session-strategy | The Claude CLI adapter's session-reuse policy: `new`, `continue`, `named`, or `pool`. |
| stepped-executor | One evaluation cycle per call, behavior-tree style (also called tick). |
| subgraph | A nested/hierarchical graph embedded as a node, invoked via the `subgraph_invoke` handler. |
| templating | Interpolation of `{inputs.x}` / `{ctx.x}` expressions into config string values (e.g. URLs, paths, headers). |
| token | A typed value carried on an edge between nodes. |
| topological-executor | Dependency-ordered parallel execution in waves via tokio (also called batch). |
| WS-handshake | Auth applied to a WebSocket connection upgrade, supported by select auth-strategies. |
