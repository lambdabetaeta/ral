---
generated_at_commit: 30ac8e07
generated_at_date: 2026-09-02
covers_paths: [ral/src/jobs.rs, ral/src/repl/host_handlers.rs]
---

# Map: repl / jobs

`ral/src/jobs.rs` is interactive job control (SPEC §11.6). `JobTable` tracks
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
`disown` — and is reaped each iteration and on exit by the [[map/repl/loop|session]].
`fg`/`bg`/`disown` name their job explicitly: each declares exactly one `Int`
id, so the checker guarantees the argument and `job_id_arg` has nothing to
default to. Declaring their arguments makes all three table entries and so
natives, and `$fg` is a value, host-captured and first-class at once, as the
nullary `jobs` already is ([[invariants/fixed-arity|fixed-arity]]). `reap`
requests continued as well as stopped statuses, so a group resumed out-of-band
by an external `kill -CONT` flips back to running (`mark_running`) rather than
reading `stopped` forever.
On exit a job group is taken down in three steps:

- gracefully — SIGTERM then SIGCONT on Unix (a frozen group must run to act on
  the request); `CTRL_BREAK_EVENT` to every group on Windows;
- given a five-second grace during which natural exits are reaped;
- then forced — SIGKILL / `TerminateJobObject`.

A parked job's pgid anchor ignores every termination signal by design
([[decisions/260902_stages-are-threads|stages-are-threads]]), so a bare
`SIGTERM -pgid` at exit would never reach it: `cleanup` instead calls
`ParkedPipeline::cancel(Terminate, shell)` on every parked job, which opens the
Ctrl-Z gate and resumes every stage — so a cancelled thread can leave and a
remembered stop cannot read as live — and then runs the collector's own
`cancel_all`: signal, grace, kill, observe. A parked pipeline's stages
therefore get the same `SIGTERM` grace a plain stopped job gets, where they
once got none. Consuming the `ParkedPipeline` this way is what finishes the
anchor (its release pipe closes, it reads EOF, `PipelineGroup::drop` reaps it). A stopped standalone external, which owns no
`ParkedPipeline`, still takes the plain SIGTERM/SIGCONT/SIGKILL ladder above.

A job also owns whatever atomic writes its members staged but have not
finished. `Escape::Stopped` carries them out of the evaluator as
`PendingWrite`s — a `tmp` path and a `target` path, no open file — and
`JobTable::settle` decides each one when the job ends: renamed onto the target
if the group leader exited 0, unlinked on every other ending, `disown` and the
exit sweep included. `settle` is the only way a row leaves the table, so a job
cannot be dropped with its writes left stranded. This is why `wait_foreground`
and `reap` keep the leader's exit status instead of draining past it.

`Escape::Stopped` is Unix-only: a foreground job stopped by SIGTSTP escapes the
[[map/core/evaluator|evaluator]] and `exec.rs` records it as a `Stopped` job.
That arm is the table's only live populator — `spawn` registers workers (below)
on every platform — so on Windows, which has no SIGTSTP analogue, the table is
empty in a live session. It still compiles and operates cfg-free: `fg` blocks
on the leader's process handle (no console handoff, `stopped_by` always
`None`), `bg` has no SIGCONT to send, and a reaped-away group's stragglers die
through the Job Object's KILL_ON_JOB_CLOSE flag.

On Unix, **`fg` resumes a parked group only through a held terminal lease**: the
controlling-terminal handoff is an unforgeable [[map/core/shell-state|TerminalLease]]
the run is given, not a predicate `wait_foreground` re-derives: it arrives as
the `&Mooring` that threads beside the shell. `Shell::terminal_lease` yields
`Some(&TerminalLease)` only when that mooring's terminal access permits and
the session owns the lease (see [[decisions/260619_terminal-lease|terminal-lease]]);
the [[map/core/io-process|ForegroundGuard]] acquires `tcsetpgrp` + the termios
snapshot only on that borrow, *before* `fg` drives the resume. Without a lease
— a non-interactive resume — there is no tty dance to do, so `fg` still
resumes and waits but skips the handoff.

**A `Job` parked by Ctrl-Z owns a live `ral_core::ParkedPipeline`, not a bare
pgid to `waitpid` on.** A pipeline's stages are threads waiting on their own
externals now, so the REPL cannot `waitpid(-pgid)` without reaping children a
stage thread still owns ([[decisions/260902_stages-are-threads|stages-are-threads]]).
`Job` therefore holds `parked: Option<ral_core::ParkedPipeline>` and is neither
`Clone` nor `Debug`; every consumer routes through the parked pipeline's own
verbs instead of a raw wait:

- `fg` drives `ParkedPipeline::resume_and_collect` (open the gate, `SIGCONT
  -pgid`, resume the same collect loop the pipeline was parked out of) after
  acquiring the `ForegroundGuard` above, and re-parks the job if a later stop
  interrupts it;
- `bg` drives the cheaper `ParkedPipeline::resume` (gate plus `SIGCONT`, no
  terminal claim) and leaves the job in the table;
- the job sweep drives `ParkedPipeline::poll`, one non-blocking probe pass,
  in place of the old `try_waitpgid`;
- session `cleanup` drives `ParkedPipeline::cancel` (above); there is no
  `kill %n` verb — `fg` then Ctrl-C ends a job, the pipeline's anchor
  witnessing the interrupt for the collector.

`disown` moves a job's `ParkedPipeline` into a session-lived `disowned: Vec<_>`
rather than dropping the table row outright, so a disowned parked pipeline is
still driven to completion (just no longer through `jobs`/`fg`/`bg`) instead of
abandoned mid-collection. A stopped standalone external — no `ParkedPipeline`,
just a remembered pgid — is the one case still resumed by a bare `SIGCONT`
and reaped by `waitpid(-pgid)`.

## Two populations, one listing

`spawn { sleep 10 }` yields an in-process handle, invisible to `jobs` until
this fold. `host_handlers.rs::render_jobs` folds *both* populations into one
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
