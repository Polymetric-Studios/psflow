# User Preferences

## Project Terminology

- Read the project terminology document (ergon/project-data/project-terminology.md) now and before starting any work.

## Time

- The current year is 2026!
- Always generate timestamps with: `date  "+%Y%m%d-%H%M%S"`.
- Never guess or make up dates or times.

## MCP

- Use the `mcp__ergon__ergon` tool if available

## Documents

- New and active documents live in ergon/active-documents.
- Move completed or obsolete documents to ergon/archived-ignore.
- Always name documents using the format `YYYYmmdd-HHMMSS-(title)` and write the file name at the top of the document.
- Always use numbered outlines for organization.
- Always use Markdown task lists for task lists.
- Never write code in documents unless explicitly asked.
- Be aware when documents such as todos, plans, README, etc. become stale and need updating.
- Never use any numbers that will rot like file counts, line numbers, etc.

## Version Control

- Never add "Co-Authored-By: Claude ..." to commit messages.
- Skip pre-commit diff when you already know the changes.

## Miscellaneous

- CONTEXT COSTS REAL $$$! ALWAYS MINIMIZE CONTEXT USAGE! BE CONCISE AND DON'T WASTE TOKENS ON THE SUPERFLUOUS!
- Always check the current date when researching APIs or anything else.
- Always read the justfile if present and remember the commands.
- Do not worry about backwards compatibility or breaking changes. We are always iterating and improving.
- When running new builds during iteration be sure to kill existing processes first.
- Always use the Ask Question tool when you have questions for me.
 
## Code

- Enforce principles: KISS, DRY, SOC, SOLID, YAGNI
- Enforce SSOT (Single Source of Truth)
- Enforce modular patterns over monolithic files.
- Adopt TDD whenever relevant.
- Code should be self-documenting by using descriptive names.
- Enforce defined constants over magic numbers and strings.
- Prefix debug logs with "[SCOPE][FILENAME]" where SCOPE is a common indicator of the current task scope for easy filtering.

## Tools

- Always use `Read` (with `limit` parameter) instead of `cat`, `head`, or `tail` via Bash.
- Always use `Grep` instead of `grep` or `rg` via Bash.
- Always use `Glob` instead of `find` or `ls` via Bash.
- Use parallel `Read` calls in a single message to read multiple files efficiently.

## Unity-specific

- Always use UWDebug.Log/Warning/Error() instead of the builtin Debug.Log/Warning/Error() so that logs flow through our own system.

## Debugging

- Polling a background process: "still running" ≠ "making progress."
  - On every "still running" re-check:
    - Find worker PIDs
      - pgrep -f <tool>
    - pgrep -P <pid> for children — cargo's wedge is usually a rustc/ld
      child, not the driver
    - Sample twice, 30s apart: ps -o pid,pcpu,stat,time -p <pid>
    - Verdict from the deltas:
      - %CPU > 0 or TIME advancing → working; reschedule
      - %CPU ≈ 0 + TIME flat + STAT=I/S → hung on a wait (lock, pipe, dead
        peer)
      - STAT=U → uninterruptible (disk/NFS); different problem, don't kill
  - When hung, check cheap causes first — most cargo wedges are environmental,
    not internal:
    - Competing cargo: pgrep -fa cargo — sibling worktree, rust-analyzer,
      leftover just build?
    - Stale lock: ls -la target/debug/.cargo-lock with no owning process → rm
      it
    - Disk: df -h on the target volume
  - Only if those are clean, diagnose the process itself:
    - spindump -notarget <pid> for a light stack snapshot (prefer over
      sample, which freezes the process ~10s)
    - lsof -p <pid> — what fds is it holding?
    - Linker spin: look for stuck ld/lld children
    - Suspected incremental-cache corruption (rare): cargo clean -p <crate>,
      never the whole workspace
  - Then act:
    - Address the cause (release the lock, free disk, kill the competitor),
      then rerun
    - Never blind-rerun — it reproduces the wedge
    - If the cause stays unknown, write the symptoms (STAT, lsof highlights,
      spindump top frames) into the tracker before restarting, or the root cause
      never gets fixed

## Workflow System

Workflows are psflow-annotated Mermaid graphs (`.mmd` files). Execution is step-by-step — the MCP returns one step at a time via `workflow_next`.

- **Start**: `ergon action="workflow_run" params={"name": "workflow-name", "inputs": {...}, "mode": "supervised"}`
- **Advance**: `ergon action="workflow_next" params={"result": "..."}` — call after every step with the step's output
- **Status**: `ergon action="workflow_status"` — shows current step and run state
- **Skip**: `ergon action="workflow_skip"` — advance past the current step without a result
- **End / cancel**: `ergon action="workflow_end" params={"action": "cancel", "reason": "..."}` (or `action="complete"`)
- **Dry-run preview**: add `"dry_run": true` to `workflow_run` to see the rendered plan without starting a run

Each step specifies its agent, model, and prompt. Deterministic steps (`ergon_deterministic` handler — e.g. `setup_folder`, `resolve_dimensions`) execute automatically with no sub-agent. Agentic steps (`agent` handler) require spawning a sub-agent.

Inter-step data flows as typed XML. The assembled prompt for each step already includes upstream results as `<context><upstream>` — the sub-agent receives full prior context without extra tool calls.

## Workflow Step Tool Split

When executing a workflow step as a sub-agent, the following overrides the general tool directives above:

- **Use raw tools for file I/O** (preferred — content lands in context directly):
  - `Read` / parallel `Read` — read files; parallel `Read` loads multiple files in one turn
  - `Edit` — modify existing files (send only the diff)
  - `Write` — create new files or full rewrites
- **Use ergon actions for everything else** (overrides `Grep`/`Glob`/`Bash`):
  - Search: `search_files` (glob), `search_content` (grep), `replace` (bulk edit)
  - Build: `run_build`, `run_tests`, `run_lint`, `run_fmt`
  - Git: `git_status`, `git_log`, `git_diff`, `git_diff_content`
- **`read_section`**: extract a specific function or struct from a large file without loading the whole thing

## Workflow Step Execution

When workflow step instructions say `Execute as agent @@@{agent}` with a `Model:` line, spawn an isolated sub-agent using the Task tool at that model. Never handle the step directly on the parent model.

Model assignment follows the step's declared agent — do not override:
- Opus: Athena (orchestration, synthesis, architecture, planning)
- Sonnet: Nemesis, Hephaestia, Clio, Metis
- Haiku: Iris (mechanical/deterministic operations)

---
## END
