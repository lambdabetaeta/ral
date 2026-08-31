---
verified_at_commit: d05cdb89
verified_at_date: 2026-08-31
anchors: [PipeNode, resolve_pipeline, StageLaunch, open_stage_routes, FinalValue::Report, run_child_eval, PipelineGroup, Launch, ChildHandle, wait_handling_stop, Escape::Stopped, wait_foreground, ForegroundGuard, TerminalLease, terminal_lease, park_on_stop, PipeYield, Capture, infer_pipeline]
---

# Pipeline execution: byte edges, process groups, and helper final values

[[design/pipelines|The design]] makes `|` a positional byte wire. Every interior
edge is an operating-system pipe from the left stage's stdout to the right
stage's stdin, alike for every pair; only the final stage may report a value.
The machine's `CompKind::Pipeline` arm
([[internals/evaluator-machine|the evaluator machine]]) reduces a single-stage
form to its inner closure and hands a multi-stage form to `PipeNode::launch`
then `PipeNode::join`, in the same rule — no frame is pushed, since nothing
runs beneath the node — resolve, launch, join (collect then finish) are the
spine below.
A stage's own stack is empty by construction: **no stage runs in the parent, so
none can be in tail position, and no frame ever crosses the wire** — only
⟨comp, scrubbed E⟩ and the wire context ride along, `E` being the pipeline
node's own lexical environment rather than `shell.env`. Ordinary application
and bind compose values in the machine and do not enter this pipeline runtime.

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
consumes these decisions; it does not re-derive a transport mode. There is no
in-process pipeline fold and no typed value channel between stages.

**Every interior route is an operating-system byte pipe, allocated from stage
position alone.** `open_stage_routes` walks the stages once: stage `i` takes its
stdin from the previous edge when there is one and from the parent otherwise,
and writes to a fresh `os_pipe` when `i + 1 < n` and to the parent otherwise.
Nothing in that loop reads a type. A non-final stage's returned value is simply
discarded (`wants_value = false`), and no returned value is ever serialised onto
an edge — the value report travels its own channel, a socketpair on Unix and an
anonymous pipe pair on Windows, never aliased with the interior pipes
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

The final-value bit is derived **once**, from two facts in one place:
`FinalValue::Report` iff `i + 1 == n` and `plan.yields` is `PipeYield::Last`.
Everything else is `FinalValue::Ignore`.

**A non-final stage cannot observe its reader's death by EPIPE.** The parent
holds a duplicate of each interior edge's read end until that edge's writer
stage is reaped, so no interior edge ever delivers a broken-pipe signal or a
write error to the stage that writes it. Instead the collector kills a
producer once its reader stage is reaped: `yes | !{ return 5 }` terminates by
that kill, on Unix and on the Windows-supported paths alike. Neither endpoint
of a pipe promises traffic, so a producer with nothing left to write for is
the ordinary case, not an error path — only the mechanism that ends it moved
from the wire to the collector.

**The final value report remains helper-staged for now.**
When the pipeline yields its last stage's value, `FinalValue::Report` selects
the helper's value report. The parent sends one `ChildEvalRequest` with the stage body and
`WireShell` snapshot; the helper evaluates it and returns the value in one
`ChildEvalResponse`, alongside status and observations. The parent does not
run a special in-process tail yet. Moving that tail into the parent is a
separate future decision ([[internals/compilation-ladder|compilation-ladder]]).

**A helper serves one `ChildEvalRequest` / `ChildEvalResponse` frame pair.**
A `HelperEval` stage uses the shared `run_child_eval` runner
(`core/src/child_eval.rs`). The parent packs the stage body plus a `WireShell`
snapshot into one request frame and gates the helper on it; the helper
reconstructs a child shell (`Shell::child_of` over the captured closure
environment), evaluates the stage, and ships one response frame carrying the
final value, status, and flat observations. It has no upstream typed-value
input: an interior producer reaches it only through its byte pipe. The grant
body evaluates locally and confines each external child per-command instead
([[internals/capability-enforcement|capability enforcement]];
[[decisions/260617_sandbox-external-children|sandbox-external-children]]). A
bundled stage takes the `Direct` arm as a `ral --ral-bundled-tool` child, and
then never reaches this runner; when a redirect, a byte-capturing audit, or a
foreground handoff rules that arm out, it comes through the helper like any
other stage and spawns that child from inside it.

**Pipelines run as one process group, held open by an anchor.** Every
multi-stage pipeline — including one whose stages are ral-implemented —
executes in a subprocess sharing one pgid the parent ral process is *not* a
member of. `PipelineGroup` (`pipeline/group.rs`) owns that pgid through a
stable *anchor* process, spawned by `prepare` because a later stage's `setpgid`
join needs a target that cannot die first. The `n ≥ 2` guard on that spawn is a
formality: the machine's `Pipeline` arm reduces a single-stage form to its
inner closure, so `PipeNode::launch` never sees one. The SIGINT-forwarding
relay is claimed on the first `spawn` — *after* `spawn_with_pgid` plus `setpgid` in `pre_exec` has put a
real child in the group — so a signal is never forwarded to a child-less pgid;
the module doc of `group.rs` states this SIGINT/relay invariant in full. Between
`prepare` and the first `spawn`, a racing SIGINT only cancels the run's
foreground scope, which the launch loop's per-stage `process::check` turns into
a prompt abort.

**A helper-evaluated stage can itself launch a pipeline.** The machine's
`Pipeline` arm is `PipeNode`'s only caller and steps in the helper exactly as in
the parent, so
a stage whose body is itself a pipeline spawns its own nested helpers. Nested
stages still receive only the byte routes and the helper control frames relevant
to their own launch; no interior typed-value descriptor is an inheritance
mechanism.

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

**Collection is an event loop over a non-blocking probe.** The collector polls
every unsettled stage with `try_settle` — the same `try_wait_handling_stop`
the single-child wait already polls — so stages settle in whatever order they
actually end, and no stage's blocking wait can starve another's news. A stage
whose reader has settled is killed (`kill_for_dead_reader`), so the cascade
runs tail-ward; a stage that stops parks the group at once, wherever it sits —
a self-stopping producer parks the pipeline rather than wedging a collector
blocked on the final stage. Each interior edge's held-open read end is dropped
once that edge's writer's observation completes, which also releases any
descendant of that edge still blocked writing into it. Verdicts fold in launch
order regardless of settle order, so which stage the collector kills when
never changes which failure the fold reports. A parked pipeline abandons its
held read ends along with its stage handles and reverts to raw OS pipe
behaviour; its verdict was already only its leader's exit, so nothing here
changes for it.

**Abort is gate-first.** A mid-launch failure SIGTERMs the pgid so whoever honours
it can leave before the drop order reaches SIGKILL, and a `PipelineBuild`
accumulator then releases every transient resource in one order: unreleased
stage gates close first (a helper parked on its job read treats EOF as the
parent's stand-down and exits), then the unconsumed `StageRoute`s (every
unspawned stage's edge ends, allocated up front by `open_stage_routes`), then
the running stage handles, then `PipelineGroup`. That order is the invariant — a
helper holding an inherited copy of the anchor channel must be let go before the
anchor is waited, or the wait deadlocks.

**Stop and resume park the whole group, and the foreground handoff orders
before the wake.** A foreground command or pipeline that takes `SIGTSTP`
becomes a parked job rather than dying. `wait_handling_stop`
(`core/src/process/signal/unix.rs`) is entered with `park_on_stop` true only on
the *interactive* foreground path — a standalone external sets it from
`fg.park_on_stop()` (`want_fg && interactive`, `runtime/command/foreground.rs`),
a pipeline stage from `plan.terminal.owns_tty()` (`pipeline/launch.rs`). A
non-interactive script that foregrounds an interactive child still takes the
terminal but has no job table to resume a parked job, so it kill-and-reaps on
stop rather than parking
([[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]).
On `WIFSTOPPED` the parking path returns `WaitOutcome::Stopped`
without killing or reaping (batch mode keeps `park_on_stop` false and runs the
legacy kill-and-reap). `RunningChild::wait` (`runtime/command/child.rs`) turns
that outcome into `Err(Break::Escape(Escape::Stopped { pgid, signal, cmd }))`
and *detaches* its pump threads — they keep draining the stopped child's pipes
and finish on their own once a later `fg` runs it to completion. For a pipeline,
the collect walk (`pipeline/collect.rs`) `SIGSTOP`s the whole `-pgid` the
moment it observes a stop, so any still-running siblings park together, and
stops probing; every stage it never observed is then `abandon()`ed so their
`Drop` does not `SIGKILL` the parked group. As `PipeNode::join` returns, `PipelineGroup`'s
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
`docs/SPEC.md` §7, §11.6; RATIONALE §"The pipe is the operating system's".
