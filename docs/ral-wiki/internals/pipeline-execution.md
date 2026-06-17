---
verified_at_commit: df36715
verified_at_date: 2026-06-17
anchors: [run_pipeline, resolve_pipeline, StageLaunch, value_edge_in, force_pipe_value, run_child_eval, PipelineGroup, spawn_with_pgid, wait_handling_stop, Escape::Stopped, wait_foreground, ForegroundGuard, startup_foreground, park_on_stop]
---

# Pipeline execution: value folds, process groups, and the resolve-time launch

[[design/pipelines|The design]] says a pipeline runs as a value fold or a
byte-process pipeline, chosen by the connecting edge's type. *Every value edge
is the one β-rule `x | f = f !{x}`, realised once*; every byte edge is a
subprocess in one group; and which of those a stage takes is frozen at resolve
time, from facts the checker already settled. `eval_pipeline` (`evaluator/comp.rs`)
reduces a single-stage pipeline to its inner computation and hands the rest to
`runtime::pipeline::run_pipeline`, whose three phases — resolve, launch,
collect — are the spine below.

**Resolve freezes a `StageLaunch` per stage from resolve-time facts.**
`resolve_pipeline` (`pipeline/resolve.rs`) reads each stage's `F[input, output]`
off the ground `Vec<Wire>` the checker wrote onto the `Pipeline` node
([[decisions/260603_ir-pipespec-annotation|ir-pipespec-annotation]]) — never
re-inferring it — and, from the unified modes, the redirects, the terminal plan,
and whether a `!{…}` audit is capturing bytes, commits one decision per stage:

- `Direct` — an external command spawned with no stage helper. A bundled tool's
  byte stage takes this arm too: its direct child is the `ral --ral-bundled-tool
  <tool>` placement chosen by the command image
  ([[decisions/260616_bundled-tools-as-exec-images|bundled-tools-as-exec-images]]);
- `HelperEval` — the stage's ral computation evaluated in a helper.

Launch reads this decision; it does not re-derive it. A whole pipeline whose every
edge is on the value channel resolves to `PipelineKind::PureValue`; anything
touching bytes is `ProcessStaged`.

**The value-edge judgment has one definition.** Whether stage *i* carries a value
on its input or output edge is `value_edge_in` / `value_edge_out`
(`pipeline/route.rs`): a value edge exists between *i* and its neighbour exactly
when both have that neighbour and the connecting mode is non-`Bytes`. Every other
site — the edge allocator that realises an edge as a byte pipe or a
serialised-value channel, the launch classifier, the helper's force decision —
asks those two predicates rather than re-testing the modes
([[decisions/260610_value-edge-locality|value-edge-locality]]).

**Every value edge realises `x | f = f !{x}` through one force.**
`force_pipe_value` (`pipeline.rs`, the module's own file) is the sole
value-level `!{x}`: a
producer's value crossing a value edge is forced exactly once — a suspended block
runs and yields its body's value; every other value (a concrete value, or a
lambda whose force is the identity) passes through. The same function is called

- in the parent fold (`run_value_fold`, for a `PureValue` pipeline — a sequential
  `f !{x}` over `call::invoke`, no process, no pipe, outside job control), and
- in the re-exec'd helper for a `ProcessStaged` value edge, via
  `run_child_eval(request, upstream, force_output)` — `force_output` derived at
  the helper's serve site from the stage's *own* value-out channel (a stage
  holding a value-out edge feeds a value consumer and forces; a final or
  byte-mode stage holds none and leaves the value as-is).

So the single thunk-deref the checker models at a value edge has exactly one
runtime mirror, parent-side and child-side ([[decisions/260609_pure-pipe-equation|pure-pipe-equation]];
[[decisions/260610_child-eval-unification|child-eval-unification]]).

**A helper serves one `ChildEvalRequest` / `ChildEvalResponse` frame pair.**
A `HelperEval` stage runs through the shared `run_child_eval` runner
(`core/src/child_eval.rs`): the parent packs the stage body plus a `WireMobile`
snapshot into one request frame and gates the helper on it; the helper
reconstructs a child shell (`Shell::child_of` over the captured closure env),
optionally reads one upstream value, applies the body `call::invoke` data-last,
forces per `force_output`, and ships one response frame carrying the final value,
status, and audit nodes. This `run_child_eval` runner is the one re-exec'd-child
eval protocol; the grant body no longer re-execs through it — a `grant` evaluates
locally and confines each external child per-command instead
([[internals/capability-enforcement|capability enforcement]];
[[decisions/260617_sandbox-external-children|sandbox-external-children]]). A
byte-only bundled stage takes the `Direct` arm as a `ral --ral-bundled-tool`
child, so it never reaches this runner.

**Byte pipelines run as one process group; the pgid anchor exists only for two
or more stages.** As soon as an edge touches bytes, every stage — including
ral-implemented ones — executes in a subprocess sharing one pgid the parent ral
process is *not* a member of. `PipelineGroup` (`pipeline/group.rs`) owns that pgid
through a stable *anchor* process, but `prepare` spawns the anchor only for `n ≥ 2`
stages: a single-stage pipeline has no later `setpgid` join to protect, so its one
child establishes the group as leader on `spawn`
([[decisions/260610_value-edge-locality|value-edge-locality]]). The SIGINT-forwarding relay is
claimed on the first `spawn` — *after* `spawn_with_pgid` plus `setpgid` in
`pre_exec` has put a real child in the group — so a signal is never forwarded to a
child-less pgid; the module doc of `group.rs` states this SIGINT/relay invariant in
full. Between `prepare` and the first `spawn`, a racing SIGINT only bumps the
handler's counter, which the launch loop's per-stage `signal::check` reads to abort.

**A helper-evaluated stage can itself launch a pipeline, so `HelperProtocol::wire`
clears absent value-channel env vars.** Bundled-tool dispatch goes through
`run_pipeline` (a single anchorless stage, `Wire::EXTERNAL`), so a `HelperEval`
child may spawn its own nested helper. An absent value-channel fd left merely unset
would ride the inherited environment into that nested helper, which would then parse
a fd number already closed in its address space; `wire` (`pipeline/protocol/common.rs`)
`env_remove`s the unused channel vars to break that inheritance.

**Out-of-process ral stages are subshells.** A helper stage's `cd`, env, or module
changes do not flow back — only the pipe contents and the final value cross the
boundary (`wants_mobile` is `false` for a stage; only the last value-typed stage
sets `wants_value`), which is what keeps job control coherent
([[design/pipelines|isolation]]).

**Windows needs no foreground gating.** There is no `tcsetpgrp` to race; the
terminal plan never selects `ForegroundExternalGroup`. The helper protocol still
runs (gate / report / value edges) over anonymous OS pipe pairs
(`pipeline/protocol/{unix,windows}.rs`); `TerminateJobObject` over a per-pipeline
Job Object makes the abort path plain RAII.

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
parked stages — POSIX keeps the pgid addressable while any member lives.
`Escape::Stopped` rides out to the REPL, which records a `Stopped` job;
[[map/repl/jobs|`JobTable`]] keys it by pgid. `try` and `audit` deliberately let
`Escape::Stopped` propagate unclassified — a parked job is not a recoverable
error — and pure-value pipelines never reach this flow, having no kernel stage to
suspend.

Resuming is where ordering becomes load-bearing. `wait_foreground`
(`ral/src/jobs.rs`) acquires a `ForegroundGuard` *first* — `tcsetpgrp(-pgid)`
plus a termios snapshot — and only *then* sends `SIGCONT` to `-pgid`, draining
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
`docs/SPEC.md` §4, §13, §18; RATIONALE §"Byte pipelines are processes; value pipelines
are folds".
