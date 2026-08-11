---
verified_at_commit: 95449d4
verified_at_date: 2026-08-10
anchors: [run_pipeline, resolve_pipeline, StageLaunch, open_stage_routes, FinalValue::Report, run_child_eval, PipelineGroup, Launch, ChildHandle, wait_handling_stop, Escape::Stopped, wait_foreground, ForegroundGuard, TerminalLease, terminal_lease, park_on_stop, PipeYield, Capture, infer_pipeline]
---

# Pipeline execution: byte edges, process groups, and helper final values

[[design/pipelines|The design]] makes `|` a positional byte wire. Every interior
edge is an operating-system pipe from the left stage's stdout to the right
stage's stdin, alike for every pair; only the final stage may report a value.
`eval_pipeline` (`evaluator/comp.rs`) reduces a single-stage form to its inner
computation and hands a multi-stage form to
`runtime::pipeline::run_pipeline`, whose three phases — resolve, launch,
collect — are the spine below. Ordinary application and bind compose values in
the evaluator and do not enter this pipeline runtime.

**Resolve freezes a `StageLaunch` per stage from resolve-time facts.**
`resolve_pipeline` (`pipeline/resolve.rs`) reads redirects, the terminal plan,
and whether a `!{…}` audit captures bytes, and turns each stage's head
resolution into one launch decision:

- `Direct` — an external command or bundled tool launched directly;
- `HelperEval` — the stage's ral computation evaluated in a helper.

No route is consulted — nor could one be, since the checked IR carries none:
a stage's classification cannot depend on where its payload lives, because the
choice must be observationally transparent. The one fact resolve carries
through is the pipeline node's own `PipeYield`, the syntax the checker wrote in
place of the last stage's route, frozen onto the `PipelinePlan`. Launch
consumes these decisions; it does not re-derive a transport mode. There is no in-process pipeline fold and no typed value channel
between stages.

**Every interior route is an operating-system byte pipe, allocated from stage
position alone.** `open_stage_routes` walks the stages once: stage `i` takes its
stdin from the previous edge when there is one and from the parent otherwise,
and writes to a fresh `os_pipe` when `i + 1 < n` and to the parent otherwise.
Nothing in that loop reads a type. A non-final stage's returned value is simply
discarded (`wants_value = false`), and no returned value is ever serialised onto
an edge — the value report travels the separate socketpair, never aliased with
the interior pipes
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

The final-value bit is derived **once**, from two facts in one place:
`FinalValue::Report` iff `i + 1 == n` and `plan.yields` is `PipeYield::Last`.
Everything else is `FinalValue::Ignore`.

**A consumer that stops reading must close promptly.** `yes | !{ return 5 }`
terminates because the non-reader's read end closes and the firehose takes
EPIPE, on Unix and on the Windows-supported paths alike. Neither endpoint of a
pipe promises traffic, so this is the ordinary case, not an error path.

**The final value report remains helper-staged for now.**
When the pipeline yields its last stage's value, `FinalValue::Report` selects
the helper's value report. The parent sends one `ChildEvalRequest` with the stage body and
`WireMobile` snapshot; the helper evaluates it and returns the value in one
`ChildEvalResponse`, alongside status and observations. The parent does not
run a special in-process tail yet. Moving that tail into the parent is a
separate future decision ([[internals/compilation-ladder|compilation-ladder]]).

**A helper serves one `ChildEvalRequest` / `ChildEvalResponse` frame pair.**
A `HelperEval` stage uses the shared `run_child_eval` runner
(`core/src/child_eval.rs`). The parent packs the stage body plus a `WireMobile`
snapshot into one request frame and gates the helper on it; the helper
reconstructs a child shell (`Shell::child_of` over the captured closure
environment), evaluates the stage, and ships one response frame carrying the
final value, status, and flat observations. It has no upstream typed-value
input: an interior producer reaches it only through its byte pipe. The grant
body evaluates locally and confines each external child per-command instead
([[internals/capability-enforcement|capability enforcement]];
[[decisions/260617_sandbox-external-children|sandbox-external-children]]). A
bundled stage takes the `Direct` arm as a `ral --ral-bundled-tool`
child, so it never reaches this runner.

**Pipelines run as one process group; the pgid anchor exists only for two
or more stages.** Every multi-stage pipeline — including one whose stages are
ral-implemented — executes in a subprocess sharing one pgid the parent ral
process is *not* a member of. `PipelineGroup` (`pipeline/group.rs`) owns that pgid
through a stable *anchor* process, but `prepare` spawns the anchor only for `n ≥ 2`
stages: a single-stage pipeline has no later `setpgid` join to protect, so its one
child establishes the group as leader on `spawn`. The SIGINT-forwarding relay is
claimed on the first `spawn` — *after* `spawn_with_pgid` plus `setpgid` in
`pre_exec` has put a real child in the group — so a signal is never forwarded to a
child-less pgid; the module doc of `group.rs` states this SIGINT/relay invariant in
full. Between `prepare` and the first `spawn`, a racing SIGINT only bumps the
handler's counter, which the launch loop's per-stage `signal::check` reads to abort.

**A helper-evaluated stage can itself launch a pipeline.** Bundled-tool dispatch
goes through `run_pipeline`, so a `HelperEval` child may spawn its own nested
helper. Nested stages still receive only the byte routes and the helper control
frames relevant to their own launch; no interior typed-value descriptor is an
inheritance mechanism.

**Out-of-process ral stages are subshells.** A helper stage's `cd`, env, or module
changes do not flow back — only the byte pipe contents, final result, and
observations cross the boundary. The final value report is enabled only for the
last stage, which keeps job control coherent
([[design/pipelines|isolation]]).

**Windows has no foreground handoff, and its pipeline spawn boundary is
creation-time.** There is no `tcsetpgrp` to race; the terminal plan never selects
`ForegroundExternalGroup`. The helper protocol still runs over anonymous OS
pipe pairs for gate and final-report frames (`pipeline/protocol/{unix,windows}.rs`), but
the parent side now writes numeric handle values into the helper environment and
admits the raw handles to `process::Launch`. The Windows launch backend lowers
that value through raw `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
under a process-wide launch mutex, so report/gate handles cross only to the
child named by that launch. Pipeline Job Object membership is likewise a launch
fact: the group is prepared before spawn, the child is created suspended,
assigned to the known job, then resumed; registration records a child already in
the job. Collection therefore waits on the process tree ral actually launched,
not on a post-spawn approximation
([[decisions/260702_windows-spawn-boundary|windows-spawn-boundary]]).

**Abort is gate-first.** A `PipelineBuild` accumulator owns every transient resource
under one drop order: unreleased stage gates close first (a helper parked on its job
read treats EOF as the parent's stand-down and exits), then the unconsumed
`StageRoute`s (every unspawned stage's edge ends, allocated up front by
`open_stage_routes`), then the running stage handles, then `PipelineGroup`. That
order is the invariant — a helper holding an inherited copy of the anchor channel
must be let go before the anchor is waited, or the wait deadlocks.

**Stop and resume park the whole group, and the foreground handoff orders
before the wake.** A foreground command or pipeline that takes `SIGTSTP`
becomes a parked job rather than dying. `wait_handling_stop`
(`core/src/process/signal/unix.rs`) is entered with `park_on_stop` true only on
the *interactive* foreground path — a standalone external sets it from
`fg.park_on_stop()` (`want_fg && interactive`, `runtime/command.rs`), a pipeline
stage from `plan.terminal.owns_tty()` (`pipeline/launch.rs`). A non-interactive
script that foregrounds an interactive child still takes the terminal but has no
job table to resume a parked job, so it kill-and-reaps on stop rather than
parking ([[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]).
On `WIFSTOPPED` the parking path returns `WaitOutcome::Stopped`
without killing or reaping (batch mode keeps `park_on_stop` false and runs the
legacy kill-and-reap). `RunningChild::wait` (`runtime/command/child.rs`) turns
that outcome into `Err(Break::Escape(Escape::Stopped { pgid, signal, cmd }))`
and *detaches* its pump threads — they keep draining the stopped child's pipes
and finish on their own once a later `fg` runs it to completion. For a pipeline,
`PipelineCollector::note_stop` (`pipeline/collect.rs`) records the stopped pgid
and `SIGSTOP`s the whole `-pgid` so any still-running siblings park together,
then the collect loop `abandon()`s the remaining stage handles so their `Drop`
does not `SIGKILL` the parked group. As `run_pipeline` returns, `PipelineGroup`'s
drop has the `ForegroundGuard` restore the terminal to the shell and
`AnchorProcess::finish` `SIGCONT` *just the anchor's own pid* (not `-pgid`), so
the anchor wakes, sees EOF on its release fd, and exits without disturbing the
parked stages — POSIX keeps the pgid addressable while any member lives. That
guard was acquired at launch by `claim_foreground` only when the run held a
terminal lease: `try_acquire(leader, lease)` takes a `&TerminalLease` whose
borrow *is* the proof ral owns the controlling terminal's foreground, so the
terminal plan and the guard ask the same authority
([[decisions/260619_terminal-lease|terminal-lease]]).
`Escape::Stopped` rides out to the REPL, which records a `Stopped` job;
[[map/repl/jobs|`JobTable`]] keys it by pgid. `try` and `audit` deliberately let
`Escape::Stopped` propagate unclassified — a parked job is not a recoverable
error — ordinary application and bind never reach this flow, having no kernel
stage to suspend.

Resuming is where ordering becomes load-bearing. `wait_foreground`
(`ral/src/jobs.rs`) acquires a `ForegroundGuard` *first* — `tcsetpgrp(-pgid)`
plus a termios snapshot, again gated on `shell.terminal_lease()` so a
non-interactive resume that holds no lease skips the tty dance but still
`SIGCONT`s and waits — and only *then* sends `SIGCONT` to `-pgid`, draining
with `waitpid(-pgid, WUNTRACED)` (EINTR-retried) until the group exits or stops
again. The `tcsetpgrp`-before-`SIGCONT` order is the invariant: a resumed member
that reads the tty before the handoff lands would hit `SIGTTIN` — children carry
the default disposition via `reset_child_signals` — and re-stop the whole group.
On a stop during the wait, `wait_foreground` `SIGSTOP`s `-pgid` so a partial stop
parks siblings together (a no-op for the Ctrl-Z case, where the kernel already
stopped every member), then restores the tty pgid and termios on the way out.

See also [[design/pipelines|pipelines]],
[[internals/evaluator-machine|evaluator-machine]],
[[internals/capability-enforcement|capability-enforcement]]; map
[[map/core/runtime|runtime]], [[map/core/io-process|io-process]],
[[map/repl/jobs|jobs]].
`docs/SPEC.md` §4, §13, §18; RATIONALE §"Pipelines follow their edges".
