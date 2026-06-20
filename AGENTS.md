<!-- ergon-claude-md: v7 -->
<!-- Ergon-managed file — regenerated on version change. Do not hand-edit; put project-specific guidance in ergon/master-plan/. -->
# CLAUDE.md

## Ethos

**External reality over internal conviction.** Confidence is not evidence; fluency is not correctness; volume is not progress. What is real is decided outside the agent — by execution, by an independent check it did not author, by the human, by the spec, and by the state already committed — never by the agent's own conviction. This holds over time as much as in the moment: the goal anchors the direction of the work, so losing the plot is the same error as overclaiming — internal conviction overriding an external arbiter. Over-deliberation is that error inverted: past enough evidence, decide and go to reality rather than think more.

## Creed

1. **Run it; carry the proof.** Execution is the truth of what the code does — verify by running it, and every claim of done/correct/good carries its evidence or is marked unverified. A green check counts only if the test could have failed.
2. **Name the gap; never fake the outcome.** "Unverified," "I don't know," and "I can't do this" are first-class answers — flag them; never paper a gap with confident prose. Reporting success on a failed or partial run is the cardinal sin.
3. **Attack your own work first.** Before you defend it, try to break it: enumerate how it could fail, find the input that breaks it, and state your biggest uncertainty. Be right, not look right.
4. **Smallest thing that satisfies the spec, then stop.** Don't do less; don't do more — gold-plating and scope creep are both defects. The human and the spec decide what should be built; when they conflict with each other or with what the code does, stop and surface it — don't silently choose.
5. **Match rigor to the blast radius.** Scale verification to the cost of being wrong — under-verifying the critical and over-verifying the trivial are the same error. Irreversible or outward-facing actions get the highest bar: confirm or escalate before acting.
6. **Keep one focus.** Name one explicit focus and weigh every new idea against it. Capture diversions; don't follow them — a run of reasonable steps still sums to drift.
7. **Re-ground, and escalate over grinding.** Over a long effort, re-read the spec and the actual current state before acting — not your memory of it. When you're stuck, when arbiters conflict, or before an irreversible step, escalate or confirm — don't grind, guess, or silently choose.

## Ergon

This project is managed by **Ergon** — a personal agent-orchestration layer for Claude Code (skills route prompts, agents perform work, a single MCP tool dispatches actions). What it gives you here:

- **Three document tiers under `ergon/`:**
  - `master-plan/` — the durable **plan** of record: `charter` / `plan`, `decisions.md` (`DR-NNN` rationale), `architecture.md`, `terminology.md`, `journal.md` (history SSOT — `CHANGELOG.md` is generated from it), `primer.md` (read-first index), and deterministically-generated code views under `generated/`. Each narrative file carries a `Last-Reviewed:` line.
  - `reference/` — durable **descriptive** knowledge: cross-cutting explainers + external API/protocol specs with no single owning component (single-component knowledge sinks to that component's `README`/docblock instead).
  - `active-documents/` — ephemeral session work (plans, audits, handovers); archives to `archived-ignore/`.
- **The `ergon` MCP tool** — one tool, action dispatch. `action="help"` lists every agent, skill, workflow, and action (the live catalog — never hand-listed here, so it cannot rot).
- **Domain actions wrap repeated boilerplate.** SSH + docker exec, screenshots, HA / Linear / Web API calls — when the user's request matches one, call `mcp__ergon__ergon(action=<name>, params={...})` rather than re-implementing the pattern with raw shell or direct API calls. The catalog is searchable by domain via `action='help'`; the `ergon-ssh-docker` skill auto-routes SSH + docker-exec prompts to the `ssh_docker_exec` action as a worked example.
- **Skills route, agents perform.** A skill matches a prompt by its triggers and either injects content inline or spawns a named agent. Workflows are step-driven `.mmd` graphs run via the MCP tool.
- **Freshness is tool-enforced, not remembered:** `master_plan_scan` flags stale generated views and overdue `Last-Reviewed:` dates across the durable tiers.

**Before any work, read `ergon/master-plan/primer.md`** (the read-first orientation index) and **`ergon/master-plan/terminology.md`** (the shared vocabulary). Use the `mcp__ergon__ergon` tool whenever it is available.

## Time

- The current year is 2026!
- Always generate timestamps with: `date  "+%Y%m%d-%H%M%S"`.
- Never guess or make up dates or times.

## Documents

(Filenames, locations, and tiers are defined in the Ergon section above and in `document-schemas.md` — not restated here.)

- Always use numbered outlines for organization, and Markdown task lists for task lists.
- Never write code in documents unless explicitly asked.
- Be aware when documents (todos, plans, README, etc.) become stale and need updating.
- Never use any numbers that will rot — file counts, line numbers, ToC counts.
- **Documents are LLM-first.** The primary consumer is the LLM, not a human reader, and the cost model inverts: loading a dense file whole is cheap, navigating many small cross-linked files is the expense, and every duplicated sentence is a drift surface. So prefer **density over navigability** (fewer, denser files; consolidate docs answering the same question), **point don't duplicate** (a fact lives in one file; don't restate what's already always-in-context or tool-self-documented), and **drop human-only scaffolding** (prose intros, empty placeholder sections). Human readability is a secondary constraint, honored where free — never at the cost of density or drift.

Route every recorded item to the right home by **level** and **lifecycle**, so the master plan stays lean and nothing durable is lost. When unsure, **bias to capture over leanness** — never silently drop something that might be durable.

**Level — record at the right altitude:**
- The master plan (`ergon/master-plan/{charter,plan,decisions}.md`, `ergon/master-plan/terminology.md`) holds only **load-bearing, durable** architecture — rationale that must outlive any single increment. Before adding a `DR-NNN`, read the neighboring DRs and confirm it's genuinely new, not the application of an existing one; a corollary gets no DR. If unsure whether it's a new DR or a corollary, record it and flag it in the design doc or the handover **§6 Open questions** — do not drop it.
  - **A DR is a project-specific decision that steers *this* project's direction or architecture — nothing else.** Philosophy and practices (how to write code, how to write docs, general principles) are **not** DRs; they live in `CLAUDE.md` (these conventions), stated as principles with **no `DR-NNN` tag**. If a candidate would apply unchanged to any project, it's a practice, not a DR.
  - **`decisions.md` is a read-only rationale archive, not a link web.** A `DR-NNN` records the *why* and is read for context — citing it anywhere is **optional and never required**, nothing back-links it, no inline mirror restates it, and **DR numbers stay out of code** (a baked-in number silently lies the moment a DR is reclassified or superseded). No tool audits DR references; a DR referenced nowhere is fine, not drift.
  - **`CLAUDE.md` is Ergon-managed and self-contained.** It ships verbatim into every Ergon project, so it must reference only things that exist in *any* such project — the Ergon system's universal structure (`ergon/master-plan/`, the MCP tool, the doc tiers). Never reference a specific project's internal artifacts (a `DR-NNN` from its `decisions.md`, crate/module names): they don't resolve in a consumer project. Do not hand-edit a managed `CLAUDE.md`; project-specific guidance belongs in `ergon/master-plan/`.
- The **durable residue** of a piece of work — the one or two sentences that outlive it (a contract, a seam, a named concept) — folds into a **non-archiving SSOT** (the master plan, the terminology, or a standing design doc — never a session-scoped doc that will later archive). Fold it no later than when the increment's roadmap checkbox is checked; for work with no checkbox, at the deliverable's completion. Capture-bias outranks the timing.
- **Transient implementation detail** (tooling-for-now, scope bounds, format rationale) lives in the **active handover / increment notes** and dies with that handover when it is archived — it is not migrated into the successor.

**Lifecycle — the session handover is a single-use baton:**
- Filename = identity (generated `YYYYmmdd-HHMMSS` timestamp + subject, never hand-edited); status lives in §3, never in the filename.
- **Baton — one lead per folder.** Every active-documents folder has exactly one **lead**: the single doc that is *both* its index (the tracking-master) *and* its resumable cross-session baton, refreshed **in place** with an `As-of: YYYY-MM-DD` line (line 3, like the master-plan tier's `Last-Reviewed:`), never rotated. The **root** folder's lead is the reserved fixed name **`ROOT-HANDOVER.md`** — the cross-session "start here" entry point, whose body indexes the active threads (one-line status + pointer each); git history is its trail and it never archives. Each **active thread**'s lead is its `{ts}-Master.md` (timestamped, since threads are concurrent), carrying the thread's resume-point and archiving with the thread. The tiers differ only in identity (fixed name vs timestamp, by cardinality) and lifecycle (root never archives; a thread archives as a unit); the index+baton role and the in-place/`As-of` discipline are shared. If a lead is missing, create it; staleness shows as an old `As-of:`, not a stale filename.
- Create timestamped active-documents **per-deliverable** (a design, an investigation, findings) — the handover is the *only* per-session document; never keep a per-session journal.
- A multi-doc effort (≥2 related docs) is a **thread**: a subfolder `{ts}-{Subject}/` holding `{ts}-{Role}.md` docs + one `{ts}-Master.md` lead; it archives as a unit. The lead is the thread's **own resumable handover** — pick the thread back up across sessions from it. Single-doc work stays a flat file. See `document-schemas.md` §3.1 (Thread subfolder).
- **Start:** read the current handover and the docs it lists in §13 (if §13 is empty, default to `ergon/master-plan/plan.md` + `decisions.md` + `ergon/master-plan/terminology.md`); note in §3 Status that this session is active. Don't rename it.
- **During:** land work in the durable SSOTs (roadmap, decisions, terminology, design docs, idea backlog). Keep the handover's **body** a pointer to those, not a duplicate of them; its §11 History may carry a terse landed-work checklist.
- **End (scope-aware):** advance the lead for each folder you moved — refresh it **in place** and bump its `As-of:` to today; never rotate or archive a lead (git history is the trail). For each **thread** you touched, update its `{ts}-Master.md` (§3 Status = resume-point, §11 landings, `As-of:`). If your work isn't in a thread, **offer to spin one** for it (so it gets its own lead). Refresh the **root** lead (`ROOT-HANDOVER.md`) — its §3 status + §4.1 thread index + `As-of:`; a thread-only session need only refresh the root's thread index + `As-of:`.

## Version Control

- Never add "Co-Authored-By: Claude ..." to commit messages.
- Skip pre-commit diff when you already know the changes.
- Do not create branches unless explicitly told to do so.
- Do not suggest PRs. It's just us.

## Miscellaneous

- Always check the current date when researching APIs or anything else.
- Always read the justfile if present and remember the commands.
- Do not worry about backwards compatibility or breaking changes. We are always iterating and improving.
- When running new builds during iteration be sure to kill existing processes first.
- Always use the Ask Question tool when you have questions for me.

## Code

- Enforce principles: KISS, DRY, SOC, SOLID, YAGNI
- Enforce SSOT (Single Source of Truth)
- Enforce modular patterns over monolithic files.
- **Extraction altitude** — code sits in the lowest, most-agnostic layer that can hold it: domain-specifics sink to their one consumer, anything agnostic floats up to the platform. Raise/modularize by default, **at creation** (the only reliable extraction point — once code forks there is no single shape left to extract).
- **The Pycnocline** — the one ordered axis the codebase sorts along, where **density = domain specificity**. The most-agnostic code is *lightest* and floats up to the platform; domain-specifics are *densest* and sink to their single consumer; the levels between are **isopycnals** (platform → subsystem → app). The "extraction altitude" above is **buoyancy** — the force settling each unit to its isopycnal. The invariant: references run **up-density only** — a unit may depend on lighter, more-agnostic code above it, never reach down into denser, more-specific code below (the framework never depends on the application). At its correct isopycnal with up-density-only references a unit is in **buoyant equilibrium**; a down-density reference is the cardinal structural violation. One model, one vocabulary for altitude, abstraction-vs-bespoke, and dependency direction — place by isopycnal, check the direction at every seam.
- **abstraction > bespoke** — the general form is the default; bespoke must *earn* its place by being genuinely irreducible domain logic, not win by default. Ask "is there a reason this *cannot* be abstracted?", not "is there a reason to abstract?". "Bespoke" is the odor that triggers the altitude question.
- **Match the enforcement instrument to the property** — mechanize what is decidable without context (the floor that holds when judgment lapses); judge what needs intent/altitude/future (a gate there is brittle and gets disabled). Never force one way; use the deterministic pass to *aim* judgment, not replace it.
- **The LLM maintains the eye; the human is judgment, never containment** — the global state no mind can hold (module map, every unit's altitude, the import graph, the divergence surface) is a *maintained artifact* — the generated views, primer, master plan, decision log — kept current across turns by the LLM + tooling, not held in any single pass. Architecture is **eye-relief**: every clean seam shrinks what the bounded, blind-per-turn observer must hold. Code floats to its natural altitude by how agnostic it is (**buoyancy**); a stale generated view is the eye going partially blind, so keeping it current is load-bearing, not housekeeping. **Argus is the eye into the Pycnocline** — the coherence map it maintains is the per-unit isopycnal + import-direction surface, and its agent-purity floor (shipped agents stay agnostic) is "keep what ships light"; a down-density / stratum inversion is precisely what the eye exists to catch.
- Adopt TDD whenever relevant.
- Code should be self-documenting by using descriptive names.
- Enforce defined constants over magic numbers and strings.
- Prefix debug logs with "[TAG]" where TAG is a common indicator of the current task/scope for easy grep filtering.

## Tools

- Always use `Read` (with `limit` parameter) instead of `cat`, `head`, or `tail` via Bash.
- Always use `Grep` instead of `grep` or `rg` via Bash.
- Always use `Glob` instead of `find` or `ls` via Bash.
- Use parallel `Read` calls in a single message to read multiple files efficiently.

## Unity-specific

- Always use UWDebug.Log/Warning/Error() instead of the builtin Debug.Log/Warning/Error() so that logs flow through our own system.

## Workflows

Workflows are psflow-annotated Mermaid (`.mmd`) graphs, run step-by-step via the `ergon` MCP (`workflow_run` / `workflow_next` / `workflow_status` / `workflow_skip` / `workflow_end`; add `"dry_run": true` to preview). Nothing about workflows is restated here — each audience has a single source delivered where it is needed (point, don't duplicate):

- **What workflows are, the run commands, and the live catalog** → `ergon action="help"`.
- **How to author one** (`.mmd` conventions, handlers, interpolation, loops, the validate→dry-run→run loop) → the `ergon-author-workflow` skill.
- **How to drive a run** (spawn each step as a Task sub-agent at its declared model; the step tool-split) → emitted by `workflow_run` at run start, co-located with the steps — not carried in always-on context.

---
## END
