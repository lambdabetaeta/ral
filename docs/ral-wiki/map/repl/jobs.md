---
generated_at_commit: deac2a81
generated_at_date: 2026-06-11
covers_paths: [ral/src/jobs.rs]
---

# Map: repl / jobs

`ral/src/jobs.rs` is interactive job control (SPEC §18). `JobTable` tracks
background and stopped *pipelines* keyed by process-group id. Every pipeline
stage is placed in its own pgid — set in each stage's `pre_exec` on Unix, via
`CREATE_NEW_PROCESS_GROUP` plus a Job Object on Windows — so signalling,
waiting, and foreground handoff always target the whole group. There is no
pid/pgid ambiguity here: only the group.

`waitpid_retry` swallows `EINTR` so an interrupted wait is never mistaken for
`ECHILD` (which would flip a live job to "gone"). The table backs the four
captured builtins ([[map/repl/plugins|host handlers]]) — `jobs`, `fg`, `bg`,
`disown` — and is reaped each turn and on exit by the [[map/repl/loop|session]].
A bare `fg`/`bg`/`disown` defaults to `most_recent_id` (the highest, == newest,
job id) per SPEC §18. `reap` waits with `WCONTINUED` as well as `WUNTRACED`, so
a group resumed out-of-band by an external `kill -CONT` flips back to running
(`mark_running`) rather than reading `stopped` forever. On exit a job group is
taken down in three steps:

- gracefully — SIGTERM, then SIGCONT so a stopped group can act on the
  termination request / Ctrl-Break;
- given a five-second grace;
- then forced — SIGKILL / `TerminateJobObject`.

`Escape::Stopped` is Unix-only: a foreground job stopped by SIGTSTP escapes the
[[map/core/evaluator|evaluator]] and `exec.rs` records it as a `Stopped` job.
Windows has no SIGTSTP analogue, so that state cannot arise spontaneously; the
table still compiles and operates cfg-free, with `fg` blocking on the leader's
handle and `cleanup` routed through the Job Object's KILL_ON_JOB_CLOSE flag.
