---
generated_at_commit: be40775f432a5cb0d3eea709624d4220add0d939
generated_at_date: 2026-07-21
covers_paths: [ral/src/jobs.rs, ral/src/repl/host_handlers.rs]
---

# Map: repl / jobs

`ral/src/jobs.rs` is interactive job control (SPEC §18). `JobTable` tracks
background and stopped *pipelines* keyed by process-group id. Every pipeline
stage is placed in its own pgid — set in each stage's `pre_exec` on Unix, via
`CREATE_NEW_PROCESS_GROUP` plus a Job Object on Windows — so signalling,
waiting, and foreground handoff always target the whole group. There is no
pid/pgid ambiguity here: only the group.

Core's typed `waitpgid_eintr` / `try_waitpgid_eintr` funnels keep `EINTR`
opaque, so an interrupted wait is never mistaken for `ECHILD` (which would flip
a live job to "gone"). Blocking and polling results have different types, and a
job stores a positive `Pgid`, not an integer to revalidate at each syscall.
Statuses remain total kernel data through rustix's transparent `WaitStatus`;
stopped, continued, exited, and signalled observations need no fallible enum decode
([[decisions/260720_total-wait-status|total-wait-status]]). The table backs the
four captured builtins ([[map/repl/plugins|host handlers]]) — `jobs`, `fg`, `bg`,
`disown` — and is reaped each turn and on exit by the [[map/repl/loop|session]].
A bare `fg`/`bg`/`disown` defaults to `most_recent_id` (the highest, == newest,
job id) per SPEC §18. `reap` requests continued as well as stopped statuses, so
a group resumed out-of-band by an external `kill -CONT` flips back to running
(`mark_running`) rather than reading `stopped` forever. On exit a job group is
taken down in three steps:

- gracefully — SIGTERM then SIGCONT on Unix (a frozen group must run to act on
  the request); `CTRL_BREAK_EVENT` to every group on Windows;
- given a five-second grace during which natural exits are reaped;
- then forced — SIGKILL / `TerminateJobObject`.

`Escape::Stopped` is Unix-only: a foreground job stopped by SIGTSTP escapes the
[[map/core/evaluator|evaluator]] and `exec.rs` records it as a `Stopped` job.
That arm is the table's only live populator — `&` registers workers (below) on
every platform — so on Windows, which has no SIGTSTP analogue, the table is
empty in a live session. It still compiles and operates cfg-free: `fg` blocks
on the leader's process handle (no console handoff, `stopped_by` always
`None`), `bg` has no SIGCONT to send, and a reaped-away group's stragglers die
through the Job Object's KILL_ON_JOB_CLOSE flag.

On Unix, **`fg` resumes a parked group only through a held terminal lease**: the
controlling-terminal handoff is an unforgeable [[map/core/shell-state|TerminalLease]]
the turn is given, not a predicate `wait_foreground` re-derives. The accessor
yields `Some(&TerminalLease)` only when the installed turn's access permits and
the session owns the lease (see [[decisions/260619_terminal-lease|terminal-lease]]);
the [[map/core/io-process|ForegroundGuard]] acquires `tcsetpgrp` + the termios
snapshot only on that borrow. Without a lease — a non-interactive resume — there
is no tty dance to do, so `fg` still SIGCONTs the whole `-pgid` and waits but
skips the handoff.

## The wart heals at the listing layer

`sleep 10 &` desugars to `spawn` — an in-process handle, invisible to `jobs`
until now. `host_handlers.rs::render_jobs` folds *both* populations into one
listing: `JobTable`'s pgid groups exactly as above, then the `shell.workers()`
snapshot it is handed, marked `[wN]` — a designator namespace of its own so it
can never collide with a pgid's `[n]`. A worker reads `running (worker)`
while live and `done (worker)` once settled but unclaimed (the POSIX-`Done`
analogue); once an eliminator observes it away it is simply absent from the
next `shell.workers()` snapshot, so the fold keeps no retention state of its
own. `fg`/`bg`/`disown` stay strictly pgid-typed — a numeric id that
resolves no pgid job answers with the correspondence (`await` is a handle's
`fg`, `cancel` its kill) rather than a bare "no such job". Both `Job`
(here) and core's `WorkerEntry` implement the small `Resident` signature
([[design/residency|residency]], `core/src/types/resident.rs`); `render_jobs`
reads `designator`/`state_label` off it for both populations rather than
hand-formatting per chapter, while `pgid`/`cmd` stay direct field reads —
the honest variance the signature leaves unflattened.

`host_handlers.rs::survivor_warning` is the other half of the same fold, the
deferred survivor warning finally landing as a registry consumer
([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]): at
session `Drop`, before `JobTable::cleanup` sweeps undisowned pgid groups
exactly as before, it composes one compact line naming every worker handle
still running — they die with the process, with no pgid to sweep, so this
is their only farewell. `None` when nothing is running; it never gates or
delays the exit it announces.
