---
status: active
generated_at_commit: 4957c6c4
---

# One reaper, one fold

**"Every blocking wait on its own thread" was how ral simulated `select` on
a platform that offers none — but the platform does offer one, `SIGCHLD` on
Unix and `RegisterWaitForSingleObject` on Windows, and every real shell owns
exactly one of it.** A single process-wide reaper now owns every child wait;
a subscriber never blocks a thread on its own child, and a stop never
reaches one at all — the reaper answers it with `SIGCONT` itself, the one
rule in one place.

## Why

[[decisions/260903_event-driven-pipeline-collector|event-driven-pipeline-collector]]
made the pipeline's fold pure, and the fold was right. What stayed wrong was
the *merge*: N threads each blocked on one source, all posting into one
channel, each with its own ownership story — who holds the `Child`, who may
signal the pid, who reaps — and its own cell for attribution. For one
pipeline: a waiter per external (`kill_cause`, a bare `pid`), two anchor
threads, a 200 ms cancel timer, `SettleOnDrop`, `EndingCell`, two event
vocabularies (`Report`/`Event`), and a `Settlement` sum that existed only
because one producer held no `&Shell`. Every review finding since has been a
seam between two of these stories, not a bug in the fold itself.

Simulating `select` with N threads is the answer to "the platform has no
`select` for this." It does, for children: a shell's whole job is
supervising them, and the OS already delivers one event stream for it. Using
anything else was doing by hand what the kernel already does once.

## Decision

- **`Reaper`, process-wide, started once.** `ChildHandle::into_watch(tx, f)`
  is the one door from a spawned child to a `Watch`: it consumes the handle,
  so no later `wait` on that pid is writable behind the reaper's back.
  `Watch::{kill, signal, reap}` are the only ways to act on a watched pid
  afterward. The subscriber names its own event type — the pipeline receives
  `Event::Ended(ix, outcome)` directly, the standalone command
  `ChildEvent::Ended(outcome)`, `spawn_detached` a bare channel over
  `WaitOutcome` — never an adapter thread, never a `Settlement`.
- **Unix**: `sigaction(SIGCHLD)`, installed once, without `SA_NOCLDSTOP` (a
  stop must reach the handler too) and with `SA_RESTART`, whose whole body
  is one async-signal-safe `write` to a self-pipe. The reaper thread blocks
  reading it and, per wake, asks each watched pid two `waitid` questions:
  `WSTOPPED | WNOHANG`, consumed and answered with `kill(pid, SIGCONT)` on
  the spot — sound here and nowhere else, since the pid is alive (it just
  reported a stop) and cannot be recycled, as only `Watch::reap` reaps,
  under the same lock the scan holds — and `WEXITED | WNOHANG | WNOWAIT`, an
  exit posted once and the pid left a zombie for `Watch::reap`. Two calls
  because `WNOWAIT` cannot be asked for exits alone: a stop must be consumed
  or it re-reports every wake; an exit must not be, or the pid is free for
  reuse before its owner has finished signalling it. A child exiting between
  the two calls is caught on the next `SIGCHLD`, which its own exit raises.
- **Windows**: `RegisterWaitForSingleObject` per watched handle, the
  callback posting the exit; no stops exist there at all.
- **Pid reuse closes structurally, not by care.** A `Watch` whose pid has
  exited but is unreaped holds a zombie the kernel cannot recycle, and
  `Watch::{kill, signal}` are the only ways to signal a pid at all. The
  pipeline collector reaps in `StageHandle::file_external_end`, called only
  after the stage handle has been taken out of `stages`, so no later
  `KillStage` can ever name a reaped pid.
  `kill_stage_by_pid`/`cont_stage_by_pid`/`signal_stage_by_pid` — a pid held
  outside a `Watch`, racy against reuse by construction — are gone.
- **Cancellation became an event.** `request_root_cancel` and
  `request_foreground_cancel` run inside signal handlers and are documented
  async-signal-safe, so nothing they call may lock or notify a condvar.
  `cancel.rs` gained one registration table beside the scope tree:
  `watch_cancel(scope, on_cancel)` arms a closure that fires once, as soon
  as the scope's cause is `Some`; a cancel that can lock scans the table
  synchronously (`CancelScope::cancel` calls `scan_cancels()` after its
  `fetch_max`), a cancel that cannot kicks the reaper's self-pipe (`reaper::
  kick()` on Unix is the same one-byte `write` the `SIGCHLD` handler does;
  on Windows, whose console handler runs on an ordinary thread, `kick` *is*
  `scan_cancels()`). The reaper thread's own wake ends with
  `scan_cancels()` too, so a kick may coalesce with a pid's own wake. Not a
  thread: the cancel watcher is a table the reaper's existing wake also
  scans, so process-global thread count stayed two — `ral-reaper` and
  `ral-deadline`.
- **Attribution moved into the fold.** The pipeline collector already knew
  what it did to whom — it emitted `KillStage(ix)`. `CollectState` now
  carries `sent: Vec<Option<CancelCause>>`, the strongest cause the
  collector itself ever sent each stage, joined by `max`; `CollectState::
  fold` — the one place `&mut Shell` reaches an external's settlement,
  since no reaper-posting closure may hold one — reads it back against what
  actually happened. Forgiveness is `sent[ix] == Some(ReaderGone)` and, for
  an external, `outcome.is_stage_kill()` too; a thread stage forgives the
  same way over its own `Break`. `drive` and `cancel_all` therefore take no
  `&Shell` at all. `Ending`/`EndingCell`, `kill_cause`, `run_pipeline_stage`,
  `Report`, `Settlement`, `Witnessed`, and the old `resolve` are gone with
  it. The standalone command's own `RunningChild::wait` shrank the same way:
  one `recv` over `ChildEvent::{Ended, Cancelled}`, no poll, no backoff.
- **A joining collector still watches its externals like an owner does**;
  its cancel is `Watch::signal(cause_signal); grace; Watch::kill()` per
  stage on Unix, `Watch::kill()` alone on Windows — the owner's protocol
  addressed per pid, established already by
  [[decisions/260903_event-driven-pipeline-collector|event-driven-pipeline-collector]]'s
  fix #4. The pgid-wide `SIGCONT` in `PipelineGroup::signal` stays: it
  reaches unwatched grandchildren the reaper's per-pid answer cannot.
- **What remains a thread, legitimately.** Stage threads — they are the
  computation, and a thread's last act sending its value is the
  scoped-thread idiom; `SettleOnDrop` stays as their panic story, now the
  only guard of its kind. The anchor's report pipe — one blocking `read` per
  owned pipeline; the anchor's own exit now comes from the reaper like any
  member's, so its dedicated waiter thread went. The reaper and the deadline
  daemon, process-wide.

Per owned pipeline with *k* externals and *t* thread stages: before, *k*
waiters + *t* stage threads + report reader + anchor waiter + cancel timer;
after, *t* stage threads + report reader. Standalone external latency to a
cancel: a 5–100 ms poll before, exact after.

## Rejected shape

A `ProcessEvent` vocabulary shared across every subscriber, rather than each
naming its own type through `into_watch`'s closure — rejected because it
would have re-created exactly the adapter-thread-and-translation seam this
plan exists to remove: the pipeline wants `Event::Ended(ix, _)`, the
standalone command `ChildEvent::Ended(_)`, and forcing them through one sum
just moves the translation from a thread into a match arm nobody needed.

## Consequences

- Six `#[cfg_attr(..., allow(dead_code))]` markers, left by the phase that
  deleted the poll loop's last unconditional caller, are gone because the
  symbols they marked are gone: `ChildHandle::{wait_handling_stop,
  try_wait_handling_stop}`, `RawChild::{wait_handling_stop,
  try_wait_handling_stop}`, and the two free functions of the same names in
  `signal/windows.rs`. `hatch.rs`'s table sweep gained `ChildHandle::
  try_reap`, a plain `try_wait` with no `WUNTRACED` — a stopped child reads
  as still running, exactly what a table with no consumer thread and no
  watch on that pid wants. `WaitPoll` is deleted from `outcome.rs`: a
  terminal-outcome reader was never handed a stop before, and now nothing
  in the type system needs to say so, because nothing produces one for a
  reader to be handed.
- `waitpid_eintr`/`try_waitpid_eintr`/`wait_blocking_eintr`/
  `classify_wait_status` are deleted — the reaper's `waitid` and
  `blocking_reap` are the one funnel now
  ([[decisions/260720_total-wait-status|total-wait-status]], superseded on
  the pid side). `waitpgid_eintr`/`try_waitpgid_eintr`, unreferenced since
  job control left, are deleted alongside them rather than carried forward
  unused.
- `spawn_detached`'s own intermediate-process wait moved onto the reaper: a
  bare channel and `Reaper::global().watch(pid, tx, identity)`, `rx.recv()`,
  `reap()`, in place of a private stop-answering loop.

## Risks accepted

- **`waitid(WNOWAIT)` on macOS.** POSIX and documented on Darwin; the
  reaper's real-child tests are the check, run on both hosts before the
  pipeline moved onto it. Fallback, unbuilt: `kqueue`
  `EVFILT_PROC`/`NOTE_EXIT` for exit (no reap) and `SIGCHLD` for stops.
- **A `SIGCHLD` handler is process-global.** Nothing else in the process
  installs one; the handler is one `write`.
- **The reaper is a single point of delivery.** Unbounded channels mean a
  slow subscriber cannot block it; its thread is `catch_unwind`-wrapped and
  restarted as a second line, so a bug in one scan cannot silently stop
  reaping every other watched pid.
- **Ordering across subscribers is per subscriber**, but every event of one
  pipeline now comes from one reaper scan in one order, so the residual
  noted in
  [[decisions/260903_event-driven-pipeline-collector|event-driven-pipeline-collector]]
  (edges of two members crossing) is gone within a pipeline. A synchronous
  `CancelScope::cancel` scan and a reaper scan run on different threads;
  their relative order is immaterial, since both effects are
  `CancelAll`-shaped and idempotent.
- **A cancel closure that blocks** stalls `CancelScope::cancel` on the
  cancelling thread, or the reaper. The two closures this plan installs are
  channel sends; the rule is the deadline daemon's, and is stated on
  `watch_cancel` itself.
- **The reaper's `SIGCONT` is the whole stop story.** A `kill -STOP` from
  outside on a watched child is undone within one scan, the declared
  semantics of
  [[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]; an
  unwatched descendant stopped the same way is the pgid owner's, as before.

## Non-goals

- No async runtime, no general I/O reactor: the reaper reads one pipe and
  calls `waitid`; the anchor's report pipe keeps its own thread.
- No per-command process groups or subreapers for a killed external's
  descendants; under Linux confinement the stage's jail cgroup takes them.
- No change to `resolve`/`route`, the verdict ranking, the anchor protocol,
  terminal ownership, or the spawn lock.
- No migration of the engine enquiry park (`core/src/engine.rs`'s 75 ms
  cancel-cause poll) or of the hatch table onto `watch_cancel`/the reaper —
  the park is a noted future `watch_cancel` subscriber, the table needs
  neither, being swept on demand with no consumer thread of its own.

## Superseded and unamended

Supersedes
[[decisions/260903_event-driven-pipeline-collector|event-driven-pipeline-collector]]
on the producer side only, as noted there: the fold, `Event`/`Effect`, and
`step`/`drive`'s shape are unchanged, only what feeds the channel. Supersedes
[[decisions/260720_total-wait-status|total-wait-status]] on the pid side, as
noted there: its typed `Pgid` vocabulary and pgid-signals-directly rule are
unchanged.

**Corrects** [[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]'s
"What remains of stop" section: the surviving stop-answering arm is now one,
in `reaper::scan_one`, not the two the pipeline collector and
`RunningChild::wait` each carried before this plan.

See [[decisions/260902_stages-are-threads|stages-are-threads]],
[[decisions/260726_cancel-is-a-join|cancel-is-a-join]],
[[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]],
[[internals/pipeline-execution|pipeline-execution]],
[[internals/cancellation|cancellation]], [[map/core/io-process|io-process]],
[[map/core/runtime|runtime]].
