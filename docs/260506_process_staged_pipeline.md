# Process-staged pipelines

This plan replaces the "auto-foreground safe mixed pipelines" idea. Do
not implement that plan. It tries to prove that in-process ral stage
threads will not touch the terminal. That is the wrong proof obligation.
The terminal speaks process groups, not Rust threads. The clean design is
to make every byte-capable pipeline stage live in the pipeline process
group.

The semantic reference point is:

```text
3a6cdce core: pipeline carries upstream as data-last arg; drop value_in side channel
```

A local snapshot exists at:

```text
dev/worktrees/ral-data-last-pipeline
```

That commit established the rule this plan must preserve:

```text
x | f  ==  f !{x}
```

More precisely: the upstream value is appended as the final argument to
the next stage invocation. This must remain true for value pipelines and
for value edges inside larger pipelines.

## Goal

Keep ral's current structural architecture:

- `analysis` resolves stage types, dispatch, edge kinds, terminal policy.
- `launch` realizes the frozen plan.
- `collect` waits, joins, records audit data, and reduces structured
  process outcomes at the boundary.
- `process.rs` and `signal.rs` keep structured wait outcomes, signal
  names, SIGPIPE forgiveness, and stopped-child diagnostics.

Change only the execution model for pipelines that touch bytes or OS
processes:

- Pure value pipelines remain the data-last fold introduced in `3a6cdce`.
- Any pipeline with a byte edge or external stage becomes an all-process
  pipeline from the kernel's point of view.
- External stages are normal child processes.
- Ral stages are helper child processes running one stage of ral code.
- All child processes in the pipeline join one process group before any
  stage code runs.
- Interactive foreground handoff is to that process group, once, before
  releasing the stages.

The result should make `cat README.md | glow -p` work whether `cat` is an
external command, bundled uutils helper, alias, handler, or ral block that
eventually runs `bat`. The reason it works should not be a heuristic about
`glow`. It should work because the terminal sees one foreground process
group containing every stage that can touch the terminal.

## Non-goals

- Do not foreground ral's main process together with stage threads.
- Do not decide safety by guessing whether an internal stage reads
  `/dev/tty`.
- Do not special-case `glow`, `less`, `fzf`, `bat`, or aliases to them as
  the main mechanism.
- Do not weaken `x | f = f !{x}`.
- Do not collapse value pipelines into byte string pipelines.
- Do not silently preserve parent-shell mutation semantics for
  process-staged ral helpers. A ral helper is a pipeline subprocess.
  Its cwd/env/aliases/modules/repl mutations do not flow back unless a
  specific channel is designed for that purpose.

## Vocabulary

Use explicit names in code. Avoid "internal" when the runtime distinction
has become more precise.

- `ExternalStage`: an OS command resolved through the normal external
  command resolver.
- `RalStage`: a ral computation that must run as a pipeline stage.
- `ValueStage`: a ral stage in a pure value pipeline. It is evaluated by
  the existing data-last fold.
- `BytePipeline`: any pipeline with at least one byte edge or external
  stage. It is realized as OS processes in one process group.
- `ValueEdge`: an edge carrying a serialized `Value`, not bytes.
- `ByteEdge`: an edge carrying bytes through an OS pipe.

## Preserve the value-pipeline rule

Keep the current pure-value fast path:

```rust
fn run_value_only(stages: &[Comp], shell: &mut Shell) -> Result<Value, EvalSignal> {
    let mut acc: Option<Value> = None;
    for stage in stages {
        acc = Some(invoke::invoke(stage, acc.take(), shell)?);
    }
    Ok(acc.unwrap_or(Value::Unit))
}
```

This is the canonical implementation of `x | f = f !{x}`. It is not a
Unix pipeline and should not be forced into job-control machinery. It is
typed value composition.

The new process-staged machinery starts only when the resolved pipeline
is not pure value internal. In current terms, that means:

- at least one stage dispatches externally, or
- at least one adjacent edge is `Edge::Bytes`, or
- a stage must run in a byte context because stdin/stdout is part of its
  computation.

State-flow rule:

- Pure value pipelines run in the parent evaluator and keep today's
  `invoke`/`return_to` behavior.
- Process-staged pipelines run ral stages as helper subprocesses. Those
  helpers are subshell-like: ordinary shell-state mutation is local to the
  helper. The only things that flow back are explicit pipeline products:
  bytes, values over value channels, structured stage reports, audit
  records, and process status.

This must be documented in `docs/SPEC.md`. It is the same tradeoff
traditional shells make for process pipelines, and it is what makes job
control coherent.

## High-level architecture

### Current problem

Today, mixed pipelines are half kernel objects and half ral-internal
threads:

```text
external child  |  ral thread  |  external child
```

The external children have process groups. The ral thread does not. If
the external group owns the terminal, ral's thread is in the background.
If ral owns the terminal, terminal programs in the external group are in
the background. Both choices are wrong for some useful pipeline.

### New shape

For every byte-capable pipeline:

```text
ral parent
  |
  +-- pipeline anchor, pgid P
  +-- stage 0, pgid P: external child or ral stage helper
  +-- stage 1, pgid P: external child or ral stage helper
  +-- stage 2, pgid P: external child or ral stage helper
```

The parent is never itself a stage. The parent only analyzes, launches,
foregrounds, waits, records, and cleans up.

For interactive foreground pipelines:

1. Create process group P.
2. Spawn all stage children into P behind a launch barrier.
3. Hand the controlling terminal to P.
4. Release the barrier so stages exec or begin evaluating.
5. Wait with `WUNTRACED`.
6. Restore terminal ownership to ral on completion or stop.

This removes the race where a terminal-sensitive program starts before
the process group owns the terminal.

## Shape of the code

The implementation should not grow into a long imperative launcher with
case logic scattered across analysis, launch, and collect. The intended
shape is algebraic:

```text
PipelinePlan -> fold(StagePlan, PipelineBuild) -> RunningPipeline
```

Launching a pipeline should read like a functional reduction over a
typed accumulator. Each stage consumes a `PipelineBuild` and returns the
next `PipelineBuild`.

Sketch:

```rust
struct PipelineBuild {
    group: PipelineGroup,
    gate: SpawnGate,
    incoming: Option<Channel>,
    pending_value: Option<Value>,
    running: RunningPipeline,
    audit: PipelineAudit,
}

impl PipelineBuild {
    fn step(mut self, stage: &StagePlan, shell: &mut Shell) -> Result<Self, EvalSignal> {
        let io = self.route(stage)?;
        let handle = self.spawn(stage, io, shell)?;
        self.running.add(handle);
        self.incoming = io.outgoing;
        Ok(self)
    }

    fn finish(mut self, shell: &mut Shell) -> Result<RunningPipeline, EvalSignal> {
        self.group.foreground_if_needed(shell)?;
        self.gate.release();
        Ok(self.running)
    }
}
```

The real types will differ, but the discipline matters:

- `analysis` computes facts once.
- `route` maps adjacent edge facts to concrete fds/value channels.
- `spawn` realizes exactly one stage.
- `step` is the only place that advances the pipeline accumulator.
- `finish` is the only place that foregrounds and releases the gate.
- `collect` consumes `RunningPipeline`; it does not repair launch-time
  decisions.

This should make the code shorter at the policy layer even though the
subprocess protocol adds machinery. The goal is not fewer total lines on
day one; it is fewer places where terminal, signal, and dispatch policy
can drift.

## Launch barrier

Add a small launch gate to the spawn path. It must work for external
children and ral stage helpers.

On Unix:

- Parent creates one pipe before launching the pipeline.
- Each child inherits the read end.
- In the child's pre-exec hook, after `setpgid` and signal reset, block
  in `read(gate_fd, &mut byte, 1)`.
- Parent spawns every stage and mirrors `setpgid`.
- Parent calls `tcsetpgrp` if foreground is needed.
- Parent closes the write end of the gate.
- Children see EOF and continue to `execve` or helper evaluation.

Only async-signal-safe operations are allowed in `pre_exec`. `read` and
`close` are fine. Do not allocate or lock there.

Abort rule: if any stage fails to spawn before the gate is released, the
parent must close the gate, kill the pipeline pgid/anchor, drain/close any
pipes it owns, wait for any children already spawned, and return the
original spawn error. Do not leave gated children blocked forever.

On Windows:

- Keep using the Job Object / process group abstraction already present.
- The exact gate may be a named event or inherited pipe. Use whichever
  fits the existing Windows spawn machinery with least special casing.

The barrier should be represented explicitly, for example:

```rust
pub struct SpawnGate { ... }
pub struct ChildGate { ... }
```

`PipelineGroup` should own the parent side. Stage spawn calls receive the
child side.

## Stable pipeline pgid

Do not rely on the first real stage remaining alive long enough to be the
process-group leader. Short producers like `cat README.md` can exit before
the pager is spawned or before the parent foregrounds the group.

Use a stable group anchor on Unix:

- Spawn a tiny helper process as the pipeline group leader.
- It joins no data path.
- It has stdin/stdout/stderr set to null unless diagnostics require
  otherwise.
- It waits until the parent tells it to exit, or until the pipeline group
  is killed.
- Its pid is the pgid used for all stage children and `tcsetpgrp`.

The anchor is not a pipeline stage and must not appear in audit output.

If the implementation chooses not to use an anchor, it must instead prove
that every later `setpgid(child, leader)` remains race-free when the first
stage exits immediately. That proof is hard. Prefer the anchor.

## Ral stage helper

Add a hidden helper mode, parallel to `--ral-uutils-helper`.

Suggested flag:

```text
--ral-pipeline-stage-helper
```

The parent serializes a `StageJob` to the helper over an inherited fd.
Use the existing serialization infrastructure where possible. Do not
invent ad hoc string formats.

`StageJob` should include:

- the stage `Comp`;
- the captured lexical environment needed to evaluate it;
- heritable dynamic shell state: cwd, env vars, capabilities, registry,
  modules, location, recursion limit, and relevant plugin context;
- source location for diagnostics;
- whether stdin is a byte pipe, terminal, or null;
- whether stdout is a byte pipe, terminal, or captured/audit sink;
- optional value input fd;
- optional value output fd;
- audit metadata needed for this stage.

The helper reconstructs a `Shell`, configures its `io`, then evaluates:

```rust
let upstream = read_value_if_present(value_in_fd)?;
let value = evaluator::invoke::invoke(&stage, upstream, &mut shell)?;
write_value_if_present(value_out_fd, value)?;
```

For byte output, ordinary writes to stdout are the output. For value
output, the helper must send the returned `Value` over the value channel
and must not stringify it onto stdout.

For byte input, stdin is fd 0. For value input, stdin is whatever the
stage would normally have in that context, usually null or terminal
depending on the plan. The value itself enters only as the data-last
upstream argument.

The helper must also write a structured `StageReport` to the parent on a
dedicated report fd:

```rust
struct StageReport {
    result: Result<Option<Value>, Error>,
    last_status: i32,
    audit_nodes: Vec<ExecNode>,
    stdout_capture: Option<Bytes>,
    stderr_capture: Option<Bytes>,
}
```

Shape, not exact fields, matters. The parent should not have to infer a
ral-stage error from the helper's exit code when the helper was able to
report structurally. If the helper dies by signal, the normal
`WaitOutcome` path applies and there may be no report.

## Nested external commands inside a ral stage helper

This is critical for aliases and handlers.

Example:

```ral
aliases: [
    cat: { |args| bat ...$args },
]
cat README.md | glow -p
```

The `cat` stage is a ral helper process. Inside that helper, the alias
invokes `bat`. `bat` must not become a new foreground job. It must remain
inside the pipeline process group and inherit the helper's stdin/stdout
wiring.

Add a job-control mode for helpers, for example:

```rust
JobControl::pipeline_child()
```

In this mode, `exec_external` must:

- not call `tcsetpgrp`;
- not create a new foreground process group;
- spawn nested external commands with inherited pgid, or explicitly join
  the helper's pipeline pgid;
- preserve the usual signal reset rules for the nested child;
- preserve redirects and audit recording.

This is the main semantic win. Aliases, handlers, and ral wrappers can be
real stages without needing fragile alias expansion in the parent.

## Edge transport

The analyzer already computes adjacent edge kinds. Keep that as the
source of truth.

Important invariant: external command stages speak bytes at pipeline
boundaries. Value edges are allowed only between ral helpers, or between a
ral helper and the parent for the final result. If a user wants a value to
feed an external command, they must encode it first (`to-json`,
`to-lines`, `to-bytes`, etc.). If a user wants bytes from an external
command to feed value code, they must decode it first (`from-json`,
`from-lines`, `from-string`, etc.). Do not smuggle values into external
argv as an implicit compatibility path in process-staged pipelines.

### ByteEdge

Use OS pipes exactly as today:

```text
stage[i].stdout -> pipe -> stage[i+1].stdin
```

The pipe connects real file descriptors, even when either side is a ral
helper. No parent pump should be needed for ordinary byte forwarding.
Parent pumps remain acceptable only for audit teeing or terminal capture,
and even then should be narrowly justified.

### ValueEdge

Use a length-delimited binary value channel:

```text
stage[i].value_out -> pipe/socket -> stage[i+1].value_in
```

Use existing `Value` serialization. Define the value boundary explicitly.
Before turning on process-staged value edges, audit whether the following
can be serialized and reconstructed correctly:

- ordinary scalars, lists, maps, variants, bytes;
- thunks and captured environments;
- plugin aliases and module state;
- handles and other live resources.

If a value cannot cross a process boundary faithfully, reject it with a
helpful error at the point it attempts to cross. Do not stringify it and
do not panic. It is acceptable for handles and other live resources to be
non-transferable unless there is a principled representation.

Prefer launching both sides with a value pipe so the consumer blocks
reading the value and the pipeline group is complete before foreground
handoff. The parent should only wait for a value before launching a later
stage if analysis proves that value is needed to construct the later
stage's argv or redirections. Ordinary data-last upstream values do not
need that; they travel over the value channel.

Do not convert value edges to strings. That would undo the typed pipeline
model.

## Analysis changes

Refactor `StageDispatch`:

```rust
enum StageDispatch {
    External(ExternalStage),
    Ral(RalStage),
}
```

Pure value pipelines can still be recognized and folded before any stage
is converted to a helper process.

The analyzer should produce a `PipelinePlan` with:

- `kind: PipelineKind::{PureValue, ProcessStaged}`;
- `specs: Vec<StageSpec>`;
- per-edge routing: `ByteEdge` or `ValueEdge`;
- terminal/job-control policy;
- audit policy;
- a stable display string for job table output.

Remove or retire the terminal-sensitive rejection plan. Once all
byte-capable stages are in the same pgid, `glow -p`, `less`, `fzf`, and
similar programs are ordinary foreground pipeline members.

The only remaining terminal-policy distinction should be:

- no terminal available: no foreground handoff;
- interactive foreground pipeline: foreground the pipeline pgid;
- background pipeline, when implemented: do not foreground.

## Launch changes

Replace "internal thread" launch with "ral helper process" launch for
process-staged pipelines.

Current:

```rust
StageHandle::Process(ProcessHandle)
StageHandle::Thread(JoinHandle<Result<Value, EvalSignal>>)
```

Target:

```rust
StageHandle::External(ProcessHandle)
StageHandle::Ral(ProcessHandle)
```

If pure value pipelines keep the sequential fold, they do not enter this
handle set.

The launch sequence should be:

1. Resolve all file descriptors and value channels.
2. Create `PipelineGroup` and Unix anchor if needed.
3. Create `SpawnGate`.
4. Spawn every stage into the group behind the gate.
5. Install signal relay only if the terminal will not deliver signals
   directly to the pipeline group.
6. Foreground the group if interactive.
7. Release the gate.
8. Enter collect.

Do not foreground after each stage. That is racy and unnecessary.

The build accumulator should own all fds until they are moved into a
stage. After each `step`, the only remaining incoming edge in the
accumulator should be the exact channel needed by the next stage. This is
the linear-resource invariant that keeps pipe leaks and hangs out of the
design.

Prefer implementing the sequence as construction plus reduction:

```rust
let build = PipelineBuild::new(plan, shell)?;
let build = plan
    .stages
    .iter()
    .try_fold(build, |build, stage| build.step(stage, shell))?;
let running = build.finish(shell)?;
```

If borrow-checking makes the literal `try_fold` awkward, keep the same
shape with a tiny `for` loop that only calls `build.step(...)`. Do not
inline routing, spawning, audit setup, gate setup, and accumulator
mutation into one large loop.

## Collect changes

Collect should wait for process handles only. There should be no ral stage
thread joins in process-staged pipelines.

Collect must merge two sources of truth:

- OS wait outcome for every process;
- optional `StageReport` from ral helper stages.

Rules:

- If a helper reports a structured ral error and exits normally, surface
  the structured error.
- If a helper is killed or stopped before reporting, surface the process
  outcome.
- If a helper reports success but exits nonzero, treat that as an
  internal helper protocol error with a clear diagnostic.
- If the final stage returns a value report, that is the pipeline value.
- If the final stage is byte-mode, trailing bytes are handled by the
  existing byte capture/terminal path.

Keep structured outcomes:

- `WaitOutcome::Exited`;
- `WaitOutcome::Signaled`;
- `WaitOutcome::StoppedThenKilled`, if still used;
- platform native fallback.

Preferred job-control behavior:

- If a foreground pipeline stops, restore the terminal to ral.
- Register the whole pipeline pgid as a stopped job.
- Do not immediately kill the process group.
- `fg` should foreground the pgid, send `SIGCONT`, and wait for the whole
  pipeline.

If full stopped-job support is too large for the first implementation,
keep the existing kill-on-stop behavior temporarily, but preserve the
structured diagnostic. The plan should not make stop handling worse.

## Signal rules

Unix foreground pipeline:

- Child processes are in pgid P.
- Terminal foreground pgid is P.
- Ctrl-C, Ctrl-Z, SIGQUIT from the terminal go directly to P.
- Ral parent does not need to synthesize those signals.
- Parent waits with `WUNTRACED`.
- Parent restores its own pgid and termios on completion or stop.

Unix noninteractive or non-foreground pipeline:

- Keep existing relay machinery where direct terminal delivery is absent.
- Parent signal handler forwards to active pipeline pgids.

Child signal dispositions:

- Preserve the current `reset_child_signals` rules.
- Reset SIGPIPE to default in external children and helper children.
- Preserve inherited ignored dispositions per the nohup rule.
- In ral stage helper mode, decide deliberately whether ral's own signal
  handler is installed. Prefer simple process semantics for SIGINT and
  SIGTERM: the signal should terminate the helper unless structured
  cleanup is needed. Do not leave helpers ignoring job-control signals.

Nested externals from helpers:

- Stay in the pipeline pgid.
- Do not claim foreground.
- Receive terminal signals directly with the helper.

## Redirections

A stage-level redirect must apply inside that stage process.

For external stages, keep the existing `resolve_command` and redirect
setup.

For ral helper stages:

- Apply stage redirects before evaluating the stage.
- A redirect on stdout overrides a byte edge, exactly as shell pipelines
  do: the next stage sees EOF.
- A redirect on stdin overrides the incoming byte edge.
- `2>&1` must be resolved inside the helper with the same semantics as
  external command redirection.

Write tests for redirects on ral stages, not just external stages.

## Audit

Audit should still describe the logical pipeline stages, not the helper
implementation detail.

Do not show `--ral-pipeline-stage-helper` as the command the user ran.
For a ral helper stage:

- record the source span / stage text;
- record nested external commands normally;
- record stdout/stderr capture according to existing caps;
- record value result when the stage returns a value.

For aliases that run externals, audit should make it clear both that the
alias stage ran and which external command it invoked.

All audit data produced inside a ral helper must flow back through the
helper report channel or a dedicated audit fd. Do not scrape rendered
stderr to reconstruct audit.

## Capabilities and sandboxing

Ral helper subprocesses must run with exactly the same effective
capabilities as the stage would have had in-process.

Do not accidentally grant the helper broad authority merely because it is
the current executable.

Checklist:

- `exec` grants must admit the helper executable only as an implementation
  detail, not as user authority to execute arbitrary ral helpers.
- nested external commands inside the helper must still be checked against
  the stage's dynamic capabilities;
- filesystem capabilities must see the same cwd and path canonicalization
  as today;
- plugin aliases must run under plugin capabilities, as today.

Reuse the sandbox IPC and uutils-helper patterns where possible.

The helper executable admission should be narrow. A capability profile
that allows `cat` must not accidentally allow arbitrary
`ral --ral-pipeline-stage-helper` execution outside the parent-created
pipeline protocol.

## Tests

Keep all existing pipeline tests. Add focused regressions.

### Data-last semantics

These must continue to pass:

```ral
5 | { |x| echo $x }
let f = { |x| return $[$x + 10] }
5 | $f
[1, 2, 3] | map { |x| return $[$x * 2] }
```

Add a mixed process-staged version where a value edge feeds a ral helper
near byte stages.

Also test that process-staged helpers are subshell-like:

```ral
let r = 1 | { |x| return $[$x + 1] }
echo $r
```

continues to work as a pure value fold, while a ral helper inside a byte
pipeline cannot persist an alias/cwd/env mutation back to the parent
unless a future explicit mechanism is added.

### Terminal programs

Use test helpers that exercise terminal behavior without depending on
`glow` being installed.

Create a small test program or shell snippet that:

- calls `tcgetpgrp` and verifies its pgid is foreground;
- optionally calls `tcsetattr` to catch SIGTTOU behavior;
- prints success.

Cover:

```ral
cat README.md | terminal-check
alias-to-cat README.md | terminal-check
echo "# title" | terminal-check
```

Where `alias-to-cat` is a ral alias that invokes an external command.

### Signal outcomes

Cover:

- normal exit 137 is not SIGKILL;
- real SIGKILL is not plain exit 137;
- SIGPIPE in non-final producer is success;
- SIGINT kills or interrupts the whole process group;
- SIGTSTP reports stop structurally, and if stopped-job support is
  implemented, `fg` resumes the pipeline.

### Race tests

The original heisenbug is a race. Add tests that make the first stage exit
immediately while the last stage needs foreground:

```ral
printf "" | terminal-check
true | terminal-check
```

These should pass repeatedly. The spawn gate and anchor are meant to make
this deterministic.

Also test failed launch cleanup: one stage should fail to spawn while an
earlier gated stage has already been created. The test should prove ral
does not hang and no child remains blocked on the gate.

### Redirects

Cover ral helper stages with:

```ral
{ echo hi } > file | wc -c
cat file | { from-lines } < other
{ echo err >&2 } 2>&1 | cat
```

Adjust exact syntax to current ral redirect semantics.

## Implementation phases

### Phase 1: preparation

- Add `PipelineKind::{PureValue, ProcessStaged}`.
- Keep `run_value_only` exactly equivalent to current behavior.
- Rename current `Internal` stage concepts where helpful, but do not
  change behavior yet.
- Add tests around `3a6cdce` semantics before touching launch.

### Phase 2: stage helper

- Add hidden `--ral-pipeline-stage-helper`.
- Define `StageJob` and binary serialization.
- Implement helper reconstruction of `Shell`.
- Run one ral stage with optional value input and optional value output.
- Return a structured `StageReport`.
- Reject non-transferable values at process boundaries with a helpful
  diagnostic.
- Unit-test helper round trips without involving terminal job control.

### Phase 3: process-staged launch

- Add `SpawnGate`.
- Add Unix pipeline anchor.
- Spawn ral helper stages instead of ral threads for non-pure-value
  pipelines.
- Put all external and helper stages in one pgid.
- Foreground once after launch and before gate release.
- Remove terminal-sensitive mixed-pipeline rejection.

### Phase 4: collect and job control

- Convert `StageHandle::Thread` out of process-staged collect.
- Wait on helper processes like external processes.
- Preserve structured wait diagnostics.
- Extend REPL jobs from single pid to pipeline pgid if implementing true
  stopped-job support.

### Phase 5: cleanup

- Delete dead mixed-thread terminal policy.
- Delete or rewrite `TerminalPlan::MixedNoForeground` and
  `RejectTerminalSensitiveMixed`.
- Update docs:
  - `docs/SPEC.md`;
  - `docs/RATIONALE.md`;
  - implementation notes if present.
- Add a short note that byte pipelines are Unix-like process pipelines,
  while pure value pipelines are typed data-last composition.

## Acceptance criteria

The implementation is acceptable only if all of these are true:

- `x | f = f !{x}` remains true for value pipelines.
- No byte-capable pipeline uses ral main-process threads as stages.
- Every byte-capable stage is in the same process group before any stage
  code runs.
- Foreground handoff happens once, to the whole group, before gate release.
- Aliases and handlers that invoke external commands work inside pipelines
  without special-case expansion.
- `cat README.md | glow -p` works when `cat` is aliased to `bat`.
- Normal exit 137, SIGKILL, SIGPIPE, SIGINT, and SIGTSTP are reported
  structurally and correctly.
- Existing capability checks and audit records remain meaningful.
- Process-staged ral helpers have explicit subshell-like state flow.
- Helper success/error reports are structured; parent code does not infer
  ral semantic failure from raw helper exit codes when a report exists.
- The old auto-foreground mixed-thread idea is gone.
- Pipeline launch has a reduction shape: a small typed accumulator, one
  stage-step operation, and a finish operation. A reader should be able
  to see where edge routing, process spawning, foreground handoff, and
  collection begin and end.
