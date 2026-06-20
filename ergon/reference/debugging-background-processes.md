# Debugging — polling a background process

Last-Reviewed: 2026-06-20

A cross-cutting operational explainer (no single owning module): how to tell whether a long-running background build/test is *progressing* or *wedged*, and what to do about it. Tuned for Rust/cargo on macOS, but the method generalizes. Migrated from the project's pre-v2 CLAUDE.md.

## The rule

**"Still running" ≠ "making progress."** On every "still running" re-check, measure progress from deltas — never assume.

## On every re-check

1. Find worker PIDs: `pgrep -f <tool>`.
2. `pgrep -P <pid>` for children — cargo's wedge is usually a `rustc`/`ld` child, not the driver.
3. Sample twice, 30s apart: `ps -o pid,pcpu,stat,time -p <pid>`.
4. Verdict from the deltas:
   - `%CPU > 0` or `TIME` advancing → working; reschedule.
   - `%CPU ≈ 0` + `TIME` flat + `STAT=I/S` → hung on a wait (lock, pipe, dead peer).
   - `STAT=U` → uninterruptible (disk/NFS); a different problem — don't kill.

## When hung, check cheap (environmental) causes first

Most cargo wedges are environmental, not internal:

- Competing cargo: `pgrep -fa cargo` — sibling worktree, rust-analyzer, leftover `just build`?
- Stale lock: `ls -la target/debug/.cargo-lock` with no owning process → `rm` it.
- Disk: `df -h` on the target volume.

## Only if those are clean, diagnose the process itself

- `spindump -notarget <pid>` for a light stack snapshot (prefer over `sample`, which freezes the process ~10s).
- `lsof -p <pid>` — what fds is it holding?
- Linker spin: look for stuck `ld`/`lld` children.
- Suspected incremental-cache corruption (rare): `cargo clean -p <crate>`, never the whole workspace.

## Then act

- Address the cause (release the lock, free disk, kill the competitor), then rerun.
- Never blind-rerun — it reproduces the wedge.
- If the cause stays unknown, write the symptoms (`STAT`, `lsof` highlights, `spindump` top frames) into the tracker before restarting, or the root cause never gets fixed.
