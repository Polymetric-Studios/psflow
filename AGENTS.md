# AGENTS.md

# User Preferences

## Time

- The current year is 2026.
- Always generate timestamps with: `date  "+%Y%m%d-%H%M%S"`.
- Never guess or make up dates or times.

## MCP

- Use the `mcp__ergon__ergon` tool if available.

## Documents

- New and active documents live in `ergon/active-documents`.
- Move completed or obsolete documents to `ergon/archived-ignore`.
- Always name documents using the format `YYYYmmdd-HHMMSS-(title)` and write the file name at the top of the document.
- Always use numbered outlines for organization.
- Always use Markdown task lists for task lists.
- Never write code in documents unless explicitly asked.
- Be aware when documents such as todos, plans, README, etc. become stale and need updating.
- Never use any numbers that will rot like file counts, line numbers, etc.

## Version Control

- Never add `Co-Authored-By: Codex ...` to commit messages.
- Skip pre-commit diff when you already know the changes.

## Miscellaneous

- Minimize context usage. Be concise and avoid superfluous detail.
- Always check the current date when researching APIs or anything else.
- Always read the justfile if present and remember the commands.
- Do not worry about backwards compatibility or breaking changes. We are always iterating and improving.
- When running new builds during iteration, kill existing processes first.
- Always use the Ask Question tool when you have questions for the user.

## Code

- Enforce principles: KISS, DRY, SOC, SOLID, YAGNI.
- Enforce SSOT.
- Enforce modular patterns over monolithic files.
- Adopt TDD whenever relevant.
- Code should be self-documenting by using descriptive names.
- Enforce defined constants over magic numbers and strings.
- Prefix debug logs with `[SCOPE][FILENAME]`, where `SCOPE` is a common indicator of the current task scope for easy filtering.

## Tools

- Always use `Read` with a `limit` parameter instead of `cat`, `head`, or `tail` via Bash when available.
- Always use `Grep` instead of `grep` or `rg` via Bash when available.
- Always use `Glob` instead of `find` or `ls` via Bash when available.
- Use parallel `Read` calls in a single message to read multiple files efficiently when available.

## Project Commands

- Build: `just build`
- Run: `just run`
- Test: `just test`
- Verbose test: `just test-verbose`
- Type-check: `just check`
- Format: `just fmt`
- Lint: `just lint`
- Clean: `just clean`
- Build WASM debugger package: `just wasm`
- Run debugger dev server: `just debugger`
- Build debugger for production: `just debugger-build`

## Workflow System

Workflows are psflow-annotated Mermaid graphs (`.mmd` files). Execution is step-by-step; the MCP returns one step at a time via `workflow_next`.

- Start: `ergon action="workflow_run" params={"name": "workflow-name", "inputs": {...}, "mode": "supervised"}`
- Advance: `ergon action="workflow_next" params={"result": "..."}`
- Status: `ergon action="workflow_status"`
- Skip: `ergon action="workflow_skip"`
- End or cancel: `ergon action="workflow_end" params={"action": "cancel", "reason": "..."}`
- Dry-run preview: add `"dry_run": true` to `workflow_run` to see the rendered plan without starting a run.

Each step specifies its agent, model, and prompt. Deterministic steps (`ergon_deterministic` handler) execute automatically; no sub-agent is needed. Guided or orchestrated steps (`ergon_skill` handler) require spawning a sub-agent.

Inter-step data flows as typed XML. The assembled prompt for each step already includes upstream results as `<context><upstream>`, so the sub-agent receives full prior context without extra tool calls.

## WFA Skill Tool Split

When executing as a WFA skill agent, the following overrides the general tool directives above:

- Use raw tools for file I/O when available:
- `Read` or parallel `Read` to read files.
- `Edit` to modify existing files.
- `Write` to create new files or perform full rewrites.
- Use ergon actions for everything else:
- Search: `search_files`, `search_content`, `replace`.
- Build: `run_build`, `run_tests`, `run_lint`, `run_fmt`.
- Git: `git_status`, `git_log`, `git_diff`, `git_diff_content`.
- Use `read_section` to extract a specific function or struct from a large file without loading the whole thing.

## WFA Step Execution

When workflow step instructions say `Execute as agent @@@{agent}` with a `Model:` line, spawn an isolated sub-agent using the Task tool at that model. Never handle the step directly on the parent model.

Model assignment follows WFA skill ownership. Do not override:

- Opus: Athena for orchestration, synthesis, architecture, and planning.
- Sonnet: Nemesis, Hephaestia, Clio, Metis.
- Haiku: Iris for mechanical or deterministic operations.
