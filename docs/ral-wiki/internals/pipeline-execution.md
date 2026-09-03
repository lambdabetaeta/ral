---
verified_at_commit: 43183ee3
verified_at_date: 2026-09-03
anchors: [PipeNode, resolve_pipeline, StageLaunch, open_stage_routes, launch_thread_stage, ThreadStage, Report, Settlement, Event, Effect, step, StageObservation, CollectState, CollectState::resolve, CollectState::sender, CollectState::all_stages_launched, CollectState::run, CollectState::drive, CollectState::try_advance, CollectState::cancel_all, PipelineGroup, PipelineGroup::prepare, PipelineGroup::joining, PipelineGroup::kill, PipelineGroup::start_witness_threads, GroupRole, Ending, EndingCell, AnchorProcess, StageGate, StagePark, StagePark::gate, StageStop, StageStop::wait_for_change, Mooring::park, StopPolicy, ParkedPipeline, ChildHandle, wait_handling_stop, wait_tracking_stops, Escape::Stopped, wait_foreground, ForegroundGuard, TerminalLease, terminal_lease, PipeYield, Capture, infer_pipeline]
---

# Pipeline execution: byte edges, one process group, threads and processes

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
none can be in tail position, and no frame ever crosses into a stage** — only
⟨comp, captured env⟩ rides along, the environment being the pipeline node's own
lexical environment rather than `shell.env`. Ordinary application and bind
compose values in the machine and do not enter this pipeline runtime.

**Resolve freezes a `StageLaunch` per stage from resolve-time facts.**
`resolve_pipeline` (`pipeline/resolve.rs`) reads redirects, the terminal plan,
and whether a `!{…}` audit captures bytes, and turns each stage's head
resolution into one launch decision:

- `Direct(ExternalStage)` — an external command or bundled tool spawned
  straight into the group as its own process, no ral in front of it;
- `Thread` — the stage's ral computation evaluated on its own OS thread, over
  a cloned `Shell`. An external head becomes a `Thread` stage too whenever a
  redirect or a byte-capturing audit rules `Direct` out; the external is then
  spawned from inside that thread instead.

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
Nothing in that loop reads a type, and nothing in it reads whether a stage is a
thread or a process — a `StageRoute` is the same value either way, and
`Thread`/`Direct` differ only in how the route is wired into an `Io` versus a
child's stdio. A non-final stage's returned value is simply discarded, and no
returned value is ever serialised onto an edge
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

**The final-value bit is derived once**, from `i + 1 == n` and
`plan.yields == PipeYield::Last`; every other stage is `is_last = false`. A
`Thread` stage simply returns its value on its `JoinHandle` (`StageOutcome`);
the collector keeps it only when `is_last` says to.

**A non-final stage cannot observe its reader's death by EPIPE.** The parent
holds a duplicate of each interior edge's read end until that edge's writer
stage is killed or reaped, so no interior edge ever delivers a broken-pipe
signal or a write error to the stage that writes it. Instead the collector
kills a producer once its reader stage is reaped: `yes | !{ return 5 }`
terminates by that kill, on Unix and on the Windows-supported paths alike.
Neither endpoint of a pipe promises traffic, so a producer with nothing left
to write for is the ordinary case, not an error path.

**Bytes reach a stage thread through `Sink::Pipe` and a `SourceReader` with a
wake.** A `Thread` stage's `Io` is built straight from its `StageRoute`:
`ByteOut::Downstream` becomes `Sink::Pipe(Arc<PipeWriter>, Arc<Wake>)`, shared
by `stdout` and `ambient` alike so a discarded statement's bytes go to one
place, and ended by the same wake as the stage's reads (below);
`ByteIn::Upstream` becomes `Source::Reader(SourceReader::pipe(r).interruptible(wake))`,
the pipe reader beside a `Wake` a blocked read polls with its fd.
A fired wake reads as EOF, never as an interrupted-read error, so `std::io::copy`
and friends cannot spin on it. `Source::reader` never *takes* a source — every
consumer, a builtin, an external's stdin, a nested stage, receives a
`dup(2)` — so a stage's stdin outlives every command that reads it and a wake
always has a referent to end; the wake itself is stripped before a child
inherits the source, so it is never a child's problem. `ByteIn::Parent` at
stage 0 resolves against the shell's own stdin the same way, with one
deliberate exception: a source with no reader on a tty-owning pipeline
resolves to `Source::Empty`, never to the controlling terminal — a thread in
the shell process reading a tty whose foreground belongs to the externals'
group would raise `SIGTTIN` against the whole shell. `!{ from-line } | cat` at
an interactive prompt therefore sees EOF, exactly what it already saw under
capture (`docs/SPEC.md` §7.6).

**Pipelines run as one process group, held open by an anchor on every
platform.** Every multi-stage pipeline — including one whose stages are all
ral-implemented — shares one pgid the parent ral process is *not* a member of.
`PipelineGroup::prepare` (`pipeline/group.rs`) spawns `ral --ral-pipeline-anchor`
before any stage exists: stderr null, stdin a pipe whose write end the parent
keeps, stdout a pipe the parent polls, the default `SIGTSTP` disposition kept
so it stops with the rest of the group. Every termination signal
(`SIGINT`/`SIGTERM`/`SIGHUP`/`SIGQUIT`) is caught rather than obeyed: the
handler writes the signal number to stdout and the anchor lives on. That
immunity is what lets the SIGINT relay install right after `prepare`, rather
than waiting for a first real stage spawn: the anchor is a genuine member
already, so a relayed signal is never forwarded to a child-less pgid.
`PipelineBuild::new` claims the foreground before any stage runs —
`claim_foreground` (`group.rs`) is still gated on a borrowed `&TerminalLease`.
With the terminal settled before user code exists, no start-gate frame is
needed.

**The anchor is the group's witness, for a stop and for a signal alike — two
dedicated threads, never polled.** The shell is not a member of the pgid, so
while the terminal belongs to the group the shell never receives the Ctrl-C or
Ctrl-Z the tty delivers to it. With an external in the group a Ctrl-C would at
least kill that external and the pipeline would drain; with none —
`!{ a } | !{ b }` at the prompt — the anchor is the only process the kernel
can reach, and it swallows the signal. `PipelineGroup::start_witness_threads`
(called once the collector's channel exists, since the anchor spawns before
it does) starts both: a report reader blocks reading the anchor's stdout to
EOF, one byte per swallowed signal, sending `Event::Witnessed(Cancelled(cause))`
— `SIGINT` maps to `Interrupt`, `SIGQUIT` to `RootAbort`, the rest to
`Terminate`, the cause the shell's own handler for that signal would apply —
and an anchor waiter blocks `waitpid(WUNTRACED | WCONTINUED)` on the anchor
itself, the sole reaper of its pid — the anchor is a member of the group like
any other, so its own stop is tracked as a level from the same two edges a
member's is, not a cell nothing ever clears — sending `Witnessed::Stopped(sig)`
on a stop and looping, `Witnessed::Continued` on a resume and looping, or
`Witnessed::Cancelled` on its own death (a plain terminating signal read by
number, same as a report) and returning. A witnessed cancel is
applied to the pipeline, not to the run's scope: `step` answers it with
`Effect::CancelAll(cause)`, which cancels every stage explicitly
(`CollectState::cancel_stages`) and signals the group once, exactly as it
would for a cancel that arrived through its own mooring, and the pipeline's
error break is what ends the run. The report reader and the anchor waiter are
separate threads over separate fds, so — unlike the old single poll's
report-outranks-stop rule — the two can race: whichever event the channel
delivers first is the one `step` answers, a swallowed signal or the anchor's
own death read off its wait outcome alike. A single-stage pipeline never
reaches any of this: the machine's `Pipeline` arm reduces it to its inner
closure inline, and `PipeNode::launch` never sees it.

**Every external spawned anywhere inside a stage joins the group.** A `Thread`
stage's `Io` carries `LaunchRole::PipelineStage(pgid)`, so any external it
spawns — at its own root, or nested arbitrarily deep — resolves
`PgidPolicy::Join(pgid)` and lands in the same group as a `Direct` stage would.
A `spawn` worker started inside a stage thread joins the group too (its `Io` is
inherited) but is minted a fresh `Mooring` with **no park** — a detached worker
is no more parked by Ctrl-Z than it is cancelled by Ctrl-C.

**A stage thread can itself launch a pipeline, which joins rather than owns.**
The machine's `Pipeline` arm is `PipeNode`'s only caller and steps identically
inside a stage thread, so a stage whose body is itself a pipeline launches its
own nested stages. That nested `PipeNode` reads `shell.io.launch_role`: for
`PipelineStage(g)` it builds `PipelineGroup::joining(g)` instead of preparing
one — no anchor, no relay, no foreground claim, and `signal`/`kill` no-op,
since only the owning top-level group may act on the pgid. Its gate comes from
`mooring.park` — the enclosing stage's own park — never from the role, so
nesting inherits the Ctrl-Z gate the way it inherits the pgid. A joining
collector never parks, signals, or escapes on its own account: on
`Event::Stopped(ix, sig)` it answers with `Effect::ForwardStop(Some(sig))`,
and on the matching `Event::Continued(ix)` with `Effect::ForwardStop(None)`,
whose interpreter writes the signal (or its resume) into its own stage's
`StageStop` — the exact cell the owning collector's own interior-stop watcher
already blocks on for a direct `Park` arm. An `Effect`, not a write `step`
performs itself: `step` stays pure over every party but this collector's own
state, so a shared cell belonging to the owner one level up is only ever
touched by the interpreter. There is nothing to "forget" the way a polled
level once needed forgetting, since the event is consumed exactly once. This
is the mechanism
by which a stop the anchor cannot witness — a single-member `SIGSTOP`, a
`SIGTTIN` under `bg` two levels down — still surfaces to the top-level owner,
one collector per nesting level. A joining group whose mooring carries no
park — the detached `spawn` worker above — has no `StageStop` cell to write
and no owner to ever `SIGCONT` it, so its collector instead answers with
`Effect::SigcontMember(ix)`: a waiter whose stop has no owner above it revives
its own child, by pid for an external, structurally by that member's own
nested `RunningChild::wait` for a further-nested thread.

**Kill for a dead reader cancels a thread and wakes it; the wake ends a write
as well as a read.** The collector's rule is unchanged in shape: a stage whose
reader stage has been observed is ended, and only that ending is forgiven. The
collector records it as the stage's `Ending` — `RalEnded(ReaderGone)`, not a
boolean — and issues it only for a stage that has just probed `Running`, so a
stage that already finished keeps its outcome and no exit status is ever
forgiven. The kill addresses the stage's pid alone, so a descendant the stage
forked outlives it and may still hold the pipe a pump of that stage reads; a
child ral ended for a dead reader therefore *detaches* its drainer threads
rather than joining them (`WaitedChild::settle`) — its remaining bytes are owed
to nobody, and the join would otherwise wait on the descendant. For
a `Thread` stage "kill" is `cancel(ReaderGone)` plus `interrupt()` (fires the
wake; Windows also `CancelSynchronousIo`). The held duplicate of the edge's
read end is kept until the stage is observed, exactly as for an external —
deliberately: ral runs with `SIGPIPE` at its default disposition (so that a
ral producing under a foreign shell's pipeline dies of it like any other
producer), and an `EPIPE` delivered to a *thread* of the shell would be a
`SIGPIPE` to the whole shell. So a blocked write is ended the way a blocked
read is: `Sink::Pipe` carries the stage's `Wake`, writes in `PIPE_BUF` chunks
each after `poll` says the pipe will not block, and a fired wake ends the
write *as success* — the bytes go nowhere, and the stage's next
`process::check` says why. A blocked read returns EOF. The stage's own
external (if it has one) is torn down by pid inside that stage's own
`RunningChild::wait`. A nested pipeline's stages are cancelled transitively
this way, each stage's own wake ending only that stage's own I/O. Forgiveness differs in one respect from an external's: a
process's wait status says whether the kill or its own `exit` ended it, a
thread's `Break` does not, so a killed thread is forgiven *whatever* it
returned — which is why `step`'s reader-gone rule fires `Effect::KillStage`
only for a writer still `Some` (unsettled) when its reader's own `Settled`
event arrives: a thread that has already finished keeps its outcome, and
`!{ echo a; exit 3 } | head -1` stays honest. Its audit fragment is folded
either way: the verdict is the collector's doing, what the stage observed
still happened. A stage thread's end is its own `Event::Settled`, sent as its
last act; a panic in the body is caught around the evaluation
(`catch_unwind(AssertUnwindSafe(..))`) and turned into the same `Settled`,
its payload becoming an `Error` carrying the stage's span. A `SettleOnDrop`
guard, constructed before the closure runs and moved into it, is each
index's own dead-man's switch: an unwind anywhere on that thread — even
before the closure body starts — drops the guard still armed, and the drop
itself posts the index's `Settled` (a generic "panicked" or "ended without
reporting" message, whichever `thread::panicking()` says). An external
stage's waiter thread carries the identical guard around its own wait.

**Stop and park is Ctrl-Z's whole story, Unix-only, and one way now, not
three.** The kernel stops what it can stop directly — the anchor and every
external in the group — and every stop arrives at the collector the same way,
as an edge on its channel: a direct external stage's own dedicated waiter
thread, blocked `waitpid(WUNTRACED | WCONTINUED)`, sends `Event::Stopped`; the
anchor's own waiter thread sends `Witnessed::Stopped` the same way. (A stage
thread's *interior* stop — a child *it* spawned stopping — is a fourth
producer, its own dedicated watcher thread blocking on `StageStop`'s condvar
rather than the cell's value directly, but arrives as the identical
`Event::Stopped`.) The anchor keeps the default `SIGTSTP` disposition
specifically so it can serve as the group's stop witness — this is what makes
`!{ a } | !{ b }`, a pipeline with no external at all, park exactly as one
with externals does. `step` answers whichever arrives by the group's
`GroupRole`, guarded by one tracked level, `parked` — a *`Foreground`* group
parks on the *first* edge of one Ctrl-Z and answers every further edge with
nothing: `sleep 10 | cat`'s single Ctrl-Z is three edges (the anchor's, and
each external's own waiter's), and without the guard the second and third
would each re-park the group right after `fg` resumes it, since a `Stopped`
already queued when `drive` returns is still waiting in the channel come the
next `recv`. The first edge gets `Effect::PauseGate` then
`Effect::SigstopGroup` (idempotent for the ordinary Ctrl-Z case) then
`Effect::Park(sig)`, which ends `drive`'s loop; `parked` clears once every
tracked level — every member's own `stopped[ix]`, and the anchor's own,
tracked by the identical two-edge rule off its `WUNTRACED | WCONTINUED`
wait — reads clear again, so a genuinely later Ctrl-Z still parks. A
*`Background`* group — batch mode, a capture, a pipeline inside a `spawn`
worker — has no job table to resume it and is cancelled with `Terminate`, an
external stage first killed by pid (`Effect::KillStoppedStage`, fired
*before* `Effect::CancelAll`'s own `SIGCONT` so the verdict still names the
stop rather than whatever the child would have done once resumed — its
waiter's own local stop tracking reclassifies the resulting death as
`StoppedThenKilled`, needing no separate settle step; distinct from
`Effect::KillStage`, the reader-gone cascade's own kill, specifically because
it must *not* raise the forgiven ending `KillStage` does — a stopped stage's
kill is not a death nothing sent it); a *`Joining`* group forwards the stop
to its owner (below). A stage thread
notices at its own pace: `process::check` consults `mooring.park`'s gate
behind an atomic fast path, so a stage blocked in a builtin finishes that call
before parking, and a stage that was mid-write or mid-read is simply blocked
at the OS level like the process it is talking to. `StopPolicy` says what
becomes of a stop that reaches a child's own `wait` unclaimed — `KillAndReap`
for batch mode, `Escape` for a top-level foreground external the REPL's job
table owns, `Park(StagePark)` for anything running inside a stage thread, an
external inside it waiting on the same child across the stop rather than
being torn down — but a *pipeline* stage's stop never reaches that decision at
all: its own dedicated waiter thread reports every edge structurally, and
`StopPolicy::KillAndReap` on its `RunningChild` is inert, never consulted.

**A parked pipeline is a value, not an abandoned process tree.** Because the
stages are threads waiting on their own children, `fg` cannot `waitpid(-pgid)`
without reaping children a stage thread still owns. `PipeNode::join`'s
`Parked` arm therefore deposits a `ParkedPipeline` — the `PipelineGroup` with
its `ForegroundGuard` and `PipelineRelay` already released (the anchor kept,
so the pgid stays joinable) plus the gate and the collector's unobserved
state — into `shell.session.parked`, keyed by pgid; the REPL's `Stopped` arm
takes it into the `Job`. `fg` re-acquires the terminal through the same
`wait_foreground` door as always, then drives
`ParkedPipeline::resume_and_collect` (opens the gate, `SIGCONT -pgid`, resumes
the same collect loop to completion or the next stop) instead of a bare
`waitpid`. `bg` opens the gate and `SIGCONT`s without touching the terminal —
neither touches `CollectState` itself: every producer thread observes the
`SIGCONT` structurally and reports its own `Continued`, so there is no level
left for `fg`/`bg` to clear by hand; the sweep drives
`ParkedPipeline::poll` → `CollectState::try_advance`, a single non-blocking
drain of whatever the channel already holds; the REPL's exit `cleanup` drives
`ParkedPipeline::cancel`, which opens the gate and then runs the same
`cancel_all` a Ctrl-C gets — a thread at the gate can leave — giving a parked
pipeline's stages the grace a plain stopped job already gets.
There is no `kill` verb: a job is ended by `fg` and Ctrl-C, which the anchor
witnesses for the collector.

**Windows has no foreground handoff and no stop to park from.** There is no
`tcsetpgrp` to race, and the terminal plan never selects
`ForegroundExternalGroup`; `ParkedPipeline` and `StopPolicy::Park` are
`cfg(unix)` in their entirety. The anchor still exists on Windows — it is what
keys the Job Object in `GROUPS` — but it holds no gate/relay role there; every
external spawned inside a stage thread still joins the Job Object through the
same `PgidPolicy::Join` resolution, assigned at creation under the suspended
create → assign → resume path.

**Collection is one channel, a pure fold, and a thin interpreter.** Every
lifecycle edge — a stage settling, a member stopping or resuming, the anchor
witnessing, a scope cancelling — is a [`Report`] one dedicated producer
thread sends: a stage thread computes its own [`StageObservation`] and sends
it as its last act; a direct external's own waiter thread
(`command::RunningChild::run_pipeline_stage`) owns that child's wait
exclusively — the collector never calls `waitpid` on a pipeline stage at
all — and reports `Stopped`/`Continued` per edge, `Settled` on the terminal
one; the anchor's two threads report `Witnessed`; a low-frequency timer, the
one left, re-checks the mooring's scope and reports `Cancelled` — the
stopgap until scopes grow their own notification, existing to be deleted.
Every index-owning producer — a stage thread's closure, an external's
waiter — carries its own `SettleOnDrop`, so its `Settled` reaches the
channel by its own send or by that guard's drop; the cancel timer and the
anchor's two witness threads hold plain sender clones and outlive every
stage, but none of them owns an index, so their long life cannot make a
stage's own end go missing. `CollectState` drops its own clone once every
stage is launched (`all_stages_launched`); a `recv` that returns `None`
regardless — every producer gone with some index never settled — is
therefore this collector's own defect, not a panic to recover, and is
treated as `Done`. `CollectState::resolve` turns a `Report` into the
[`Event`] `step` folds over — the one place `&Shell` reaches an external's
settlement (audit synthesis, exit-hint lookup, sandbox-denial augmentation),
since no producer thread may hold one. `step` is pure over `CollectState`: it
never blocks, signals, or touches a process, only inspecting the observation
vector, the group's role, and the tracked stop levels, and returns the
[`Effect`]s a thin interpreter (`CollectState::run`) performs — so the whole
corner-case space is a transition table, testable by feeding `step` a
sequence of events and asserting the effects, no process, sleep, or retry
loop needed. `drive` is `loop { for e in step(&mut st, rx.recv()?) { run(e) } }`
— no interval, no backoff, exact latency, zero idle CPU; `ParkedPipeline::poll`
is `CollectState::try_advance`, the same fold over a non-blocking drain.
`step`'s reader-gone rule — on `Event::Settled(ix)`, if stage `ix - 1` still
holds a stage handle and feeds the pipe, emit `Effect::KillStage(ix - 1)` —
is unchanged in content from the old tail-first rescan, fired now by the
event that justifies it: a stage that already finished keeps its outcome.
A stage that stops — a thread's own interior `StagePark` cell (itself watched
off a condvar, not polled, by one more dedicated thread per thread stage) or
an external's own waiter's report — is answered at once, wherever it sits.
Forgiveness for a killed thread (whatever `Break` it was about to return,
forgiven, its audit fragment kept) is applied where `Event::Settled` is
filed, reading the stage's own `EndingCell`. Each interior edge's held-open
read end drops once that edge's writer's observation completes, which also
releases any descendant of that edge still blocked writing into it. Verdicts
fold in launch order regardless of settle order, so which stage the
collector kills when never changes which failure the fold reports.

**Teardown is kill-first.** A pipeline that ends before every stage has been
observed ends by one of two mechanisms, and both put the death of what this
collector launched before anything that blocks. `CollectState::cancel_all` is
the one that observes on purpose — a cancel, a witnessed signal, a stop with
no job table, the REPL's exit — and spells the order out for both an owning
and a joining group: (1) `cancel_stages` delivers the cause to each stage —
for an external, its own pid gets the cause's catchable signal then
`SIGCONT`, on top of the `kill_cause` its waiter attributes the ending to —
and `signal` sends the same pair to `-pgid`, a no-op for a joining group,
which has no pgid of its own; (2) a bounded grace of at most `TEARDOWN_GRACE`
(500 ms, shared with a standalone child's `terminate_group`) as a single
blocking `recv_timeout` on the deadline, not a probe loop, filing only the
`Settled` events it drains — every other kind is moot once the whole group is
already dying, and is discarded; (3) the kill — `group.kill()` (`SIGKILL
-pgid`) for an owning group, or `kill_live_externals` (a pid-wise `SIGKILL`
over every stage still `Some`) for a joining one, which has no pgid to
`SIGKILL` either; (4) a further blocking drain, unbounded, for whatever the
grace did not already account for — a cancelled and killed member having
nowhere left to block; (5) the anchor last, in `Drop`, after every stage
handle has gone, its own two threads joined rather than reaped directly (its
report reader's read end closes only once it sees the anchor's own EOF, so it
is never dropped while the anchor could still `SIGPIPE` on a stray write into
it).

`CollectState::drop` is the other, and it covers every forced end that drops
rather than observes: a launch that failed part-way, an unwind between launch
and fold, a parked pipeline nobody resumed. When a stage is still unobserved —
a condition derived from what the collector already knows, not a flag a
caller must set — it kills the owned pgid, or, for a joining collector with no
pgid of its own, its own live externals by pid; either way the descendants
below it die before the handles join. Every stage observed is the ordinary
end, which a `spawn` worker that joined the pgid outlives.

Killing before joining is what makes the joins terminate: a stage's own kill
reaches its pid alone, and a pumped descendant that survived it would hold the
pump's pipe open forever. The two graces nest without adding — the group's
`SIGKILL` ends whatever a stage's own `grace_poll` is waiting on. A joining
group's own verb set, `signal` and `kill`, is a no-op either way — it has no
pgid to send either to — so its half of teardown is per pid: `cancel_stages`
signals, `kill_live_externals` kills; descendants of a killed external are the
group owner's, as for any stage's own kill.
`PipelineResources`', `PipeNode`'s and `ParkedPipeline`'s field orders carry
the drop half: routes, then the collector, then the group. The collector's
kill must reach the pgid it named, and it is the live anchor that keeps that
pgid from being reused; a stage thread parked on its own gate must also be
given the chance to leave before the anchor is waited, or the wait deadlocks.
Windows has no polite signal and no grace: the Job Object kill is the whole of
it, and `Drop` releases the group's `GROUPS` entry once the anchor is reaped.

**The terminal lease, and where it goes on park.** A foreground pipeline that
takes `SIGTSTP` becomes a parked job rather than dying. On the way into
`ParkedPipeline`, `PipelineGroup::release_foreground_and_relay` drops the
`ForegroundGuard` (restoring the shell's own pgid and termios, as its `Drop`
already does) and the `PipelineRelay`, while keeping the anchor so the pgid
stays joinable across the park — miss this step and the REPL's next tty read
raises `SIGTTIN` and stops ral itself. `Escape::Stopped` rides out to the
REPL exactly as before, recorded as a `Stopped` job keyed by pgid
([[map/repl/jobs|`JobTable`]]); `try` and `audit` let it propagate
unclassified, a parked job not being a recoverable error.

Resuming is where ordering stays load-bearing. `wait_foreground`
(`ral/src/jobs.rs`) acquires a `ForegroundGuard` *first* —
`shell.terminal_lease(mooring).and_then(|lease| ForegroundGuard::try_acquire(pgid, lease))`
— and only *then* opens the gate and sends `SIGCONT`, so a resumed member that
reads the tty before the handoff lands would hit `SIGTTIN` and re-stop the
group; the `tcsetpgrp`-before-`SIGCONT` order is the invariant this protects.
A stage thread parked on its own gate wakes the moment `StageGate::resume`
fires, needing no terminal handoff of its own — only its own subsequent reads
or writes to a foreground device do.

See also [[design/pipelines|pipelines]],
[[internals/evaluator-machine|evaluator-machine]],
[[internals/capability-enforcement|capability-enforcement]],
[[decisions/260902_stages-are-threads|stages-are-threads]]; map
[[map/core/runtime|runtime]], [[map/core/io-process|io-process]],
[[map/repl/jobs|jobs]].
`docs/SPEC.md` §7, §7.6, §11.6; RATIONALE §"The pipe is the operating system's".
