# Changelog

_Generated from `ergon/master-plan/journal.md` by the `generate_changelog` action — do not edit by hand; edit the history SSOT and regenerate._

Format: [Keep a Changelog](https://keepachangelog.com/). Entries tagged `[capability]` change what the project can or can't do; `[breaking]` marks breaking changes; `(DR-NNN)` points to the deciding record.

## [Unreleased]

### Added
- [capability] OpenRouter (and any OpenAI-wire provider — OpenAI/Groq/Together/local) via the generic `openai_compat` adapter; arbitrary-model access through per-node `config.model`. (DR-005)
- `anthropic_api` adapter emits `output_config.format` for structured (JSON-schema) outputs.
- `claude_terminal` resume mode + child-env seam on `SessionOptions`.
- `llm_call` `config.output_key` names the result port.
- `llm_call` `template_render` config for verbatim prompts.
- [capability] `claude_workflow` handler drives the real `claude` TUI headless over a PTY, with approval-dialog routing (in-session file-channel `approval=ask`) and configurable remote control. (terminal feature)
- [capability] `loop` handler — accumulate with `until` / `until_dry` / `max`; the loop family (map + subgraph_invoke + loop) wired into psflow-run. (DR-006)
- [capability] `map` handler — data-driven fan-out over a runtime list, concurrency-capped and order-preserved.
- [capability] psflow-run personal runner: named-graphs with runtime-inputs, LLM adapter wiring, run-records, notify-on-failure, cross-run-state (scheduled idempotency), config SSOT + graph discovery + fail-fast validation.
- [capability] psflow-run trigger bridge / listen-mode (Composio `dev listen` → graph dispatch), incl. an org-free receive path for personal accounts.
- Native Composio handler (CLI-wrapping, structured outputs) with record/replay + TTL tool-response-cache; API key read from the macOS keychain.
- `schema()` for all default handlers + a drift guard.
- Reactive WS handshake on the `ws` handler and `cookie_jar` CSRF cookie→header echo (auth-strategy gaps closed).
- [capability] Engine foundation (through "Phase 3"): the petgraph-backed `graph` data model with portable serde (DR-002), the annotated-Mermaid single-file format with round-trip load/export (DR-001), the feature-gated core/runtime split (DR-003), swappable executors (DR-006), and the `AiAdapter` abstraction (DR-005).

### Changed
- [capability] Isolated Composio as a removable integration and made the core provider-neutral — the engine vs psflow-run app split. (DR-004)

### Fixed
- Debugger wasm node ordering is now deterministic — co-line node definitions get a `(definition.from, id)` sort tie-break (was HashMap-iteration-order flaky).
