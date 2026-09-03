---
status: active
generated_at_commit: 0c6ec335
---

# ral does not suspend

**A stopped child is resumed, at once, by whoever waits on it: `Stopped →
SIGCONT`, one rule, every role, every platform that has stops.** There is no
park, no job table, no `fg`/`bg`/`jobs`/`disown`, no Ctrl-Z gate for stage
threads, no stop witnessed by the anchor. Background work is `spawn`, which
already returns a value where `bg` returned a line in `jobs`.

## Why

Stop/continue job control is the 1980 BSD answer to "I started something
long in the foreground and want my prompt back" on a terminal you could not
open another of. None of ral's three programs need that: exarch never
suspends anything it runs, synod runs the shell in a VM, and a REPL user has
`spawn`, a second window, and Ctrl-C. It was also, empirically, the single
largest source of complexity and defects in ral's process layer — every
review finding logged against the pipeline collector this year was a
stop-tracking seam: a level with one edge, a stale edge after settlement, a
`parked` latch that never cleared (see the "risks accepted" and "an early
draft got exactly this line wrong" entries in
[[decisions/260903_event-driven-pipeline-collector|event-driven-pipeline-collector]]).

The record elsewhere agrees. fish implements job control in full, on one
reaper and one job table, and it has been a perpetual defect source for
expert maintainers (fish issue #3952: `SIGTTOU`/foreground-restore races),
to the point of disabling job control outside interactive mode entirely
(fish PR #8021). nushell shipped for years with none of it, and the failure
mode of doing nothing — Ctrl-Z stops the foreground program with no way to
resume it (elvish #988 is the identical bug) — eventually forced a minimal
freeze/unfreeze in nushell 0.103 (PR #14883), which promptly grew demand for
more (`#15196`, `#15202`: resume without refocusing, a `jobs` list). Nobody
in this small sample has found a middle point that stays small.

The design space this leaves is binary, not a dial. Either stops are
tracked — which costs the gate, the levels, the park, and the table whether
or not `bg`/`fg` exist as commands, because the expense is *tracking* a
stop, not *resuming* one — or stops do not exist here at all. The second,
done right, is one row of the collector's fold: an ownerless stop answered
with `SIGCONT` and nothing else.

## The mechanism, and why it is the one chosen

Clearing `VSUSP` on the tty is not enough by itself: `vim` reads a literal
`^Z` keystroke and calls `raise(SIGTSTP)` on itself regardless of the tty's
special-character table, so a "stop cancels the pipeline" rule would kill
the editor on a habitual keystroke — elvish's bug, with worse consequences,
and exactly what drove nushell toward freeze/unfreeze instead.

Answering every stop with `SIGCONT` is total, and that totality is the
point: `vim` stops, is resumed within the same wait loop, and redraws —
Ctrl-Z visibly does nothing, which is the honest message for a shell that
does not park. `kill -STOP` sent from outside is undone the same way,
correctly, for the same reason. `SIGTTIN` (a background process touching
the controlling tty) would in principle cycle stop → `CONT` → re-read → stop
again, but with no `bg` there are no backgrounded, tty-attached pipelines to
begin with: a `spawn` worker never receives the tty (`route_parent_stdin`
gives it `Null`/`Empty` instead), so the cycle's precondition cannot arise.
The invariant lives where the routing decision is made, not where the signal
would land.

The anchor goes one step further and ignores `SIGTSTP`/`SIGTTIN`/`SIGTTOU`
outright (`core/src/runtime/pipeline/group.rs`), so it essentially never
stops and never needs resuming — one fewer party the one rule has to reach.

What remains of "stop" in the code is exactly two answers to the same
question, not a subsystem:

- `RunningChild::wait` (`core/src/runtime/command/child.rs:269`) answers a
  stop with `SIGCONT` inline and keeps polling; the same shape covers
  `run_pipeline_stage` (`child.rs:487`) for a stage inside a pipeline.
- `WaitOutcome` (`core/src/process/outcome.rs:137`) says how a child ended;
  the adjacent `WaitPoll` (`outcome.rs:159`) is the poll-time sum,
  `Stopped(Signal) | Done(WaitOutcome)` — a stop is a value a poll can
  return, structurally excluded from ever being a value a wait *settles*
  on. The impossible arm — a completed wait reporting a stop — no longer
  type-checks into existence; it was previously reached only by
  `unreachable!()`.

## Consequences

**Deferral goes with the park it existed for.** A write could formerly
outlive the frame that staged it only because a stopped pipeline could sit
parked, unresolved, for an arbitrary human-scale interval; once no wait can
stall on a stop, no write can either. `WriteFate::Defer`,
`WriteOutcome::Deferred` (and its wire word `"deferred"`), and `StagedWrite`
are gone. `PendingWrite` (`core/src/runtime/command/redirect.rs:63`) now
carries its own abandoning `Drop` — commit-by-value on an `Option<Staged>`,
so the ordinary drop path is a no-op once `commit`/`abandon` has already
emptied it, and the only remaining path is "nobody committed, so clean up
the temp file."

**`GroupRole` collapses to a bool.** `Foreground` versus `Background` was a
distinction job control drew — a background job doesn't own the terminal,
so `fg` exists to promote it. With no promotion, the only question left is
`owned: bool`, and terminal ownership itself moves to whichever site holds
the `TerminalPlan` that decides it, rather than being read off the role.

**What stays, because it was never about stops.** `ForegroundGuard` /
`tcsetpgrp` — `vim` still needs the terminal, whether or not it can ever be
stopped and resumed by a job table. `TerminalLease`. `PipelineRelay`
(Ctrl-C). The anchor as pgid-keeper and Ctrl-C witness. `PgidPolicy`.
`TtyInputPermit`. Cancellation with grace. Thread stages. The collector's
fold itself, which shrank by exactly the rows this decision removed rather
than being replaced.

## Risks accepted

- **A user who genuinely wants `fg`.** The answer today is `spawn` for work
  meant to run in the background from the start, and Ctrl-C followed by a
  re-run under `spawn` for work that was not. If this proves wrong in
  practice, nushell's minimal freeze/unfreeze-to-foreground is the fallback
  shape — but it buys back the gate and the levels, so it should be resisted
  until real use, not speculation, demands it.
- **A program that must be stopped** — a debugger target, `kill -STOP` for
  inspection from outside. Not a shell's job under this decision: `spawn`
  it, and inspect it from outside ral with the operating system's own
  tools.
- **`SIGTTIN` cycling** is impossible only as long as nothing ral launches
  reads the controlling tty from a non-foreground process group.
  `route_parent_stdin` and `ForegroundDecision` enforce that today; the
  invariant is named here because it is the one precondition the whole
  mechanism leans on, and it is enforced at the routing site, not the
  signal site.
- **Windows** has none of this — no `SIGTSTP`, no process groups in the
  Unix sense — and loses nothing by the decision; several `cfg(unix)`
  branches simply disappear rather than needing a Windows equivalent.

## Non-goals

- No per-command process groups, subreapers, or cgroups for an unconfined
  run; a killed external's descendants remain the group owner's to end, as
  in every POSIX shell.
- No change to terminal ownership (`tcsetpgrp`), the terminal lease, or
  Ctrl-C handling — those are the parts of "job control" that were never
  about stop/continue.

## Superseded and unamended

Supersedes, in part,
[[decisions/260820_a-stage-ral-stopped-has-no-failure|a-stage-ral-stopped-has-no-failure]]:
its forgiven-death rule for a reader-gone kill is unchanged, but its two
clauses describing a *parked* pipeline no longer hold — there is no park to
abandon held read ends, and a stop no longer wedges a collector until `fg`.

See [[decisions/260902_stages-are-threads|stages-are-threads]],
[[decisions/260903_event-driven-pipeline-collector|event-driven-pipeline-collector]],
[[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]],
[[decisions/260619_terminal-lease|terminal-lease]],
[[internals/pipeline-execution|pipeline-execution]],
[[internals/cancellation|cancellation]], [[map/core/io-process|io-process]],
[[map/repl|repl]]; `docs/SPEC.md` §11.6, §7.6.
