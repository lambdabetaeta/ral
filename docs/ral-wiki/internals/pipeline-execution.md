---
verified_at_commit: ccb05833
verified_at_date: 2026-09-05
anchors: [PipeNode, resolve_pipeline, resolve_launch, StageLaunch, open_stage_routes, launch_thread_stage, ThreadStage, PipelineBuild, StageHandle::cut, Slot, Event, Event::Witnessed, SettleOnDrop, Effect, step, StageObservation, StageEnd, CollectState, CollectState::fold, CollectState::slot, CollectState::all_stages_launched, CollectState::run, CollectState::drive, CollectState::cancel_all, grace_signal, PipelineGroup, PipelineGroup::prepare, PipelineGroup::joining, PipelineGroup::kill, AnchorProcess, ChildHandle, into_watch, watch_cancel, Watch, ForegroundGuard, TerminalLease, terminal_lease, PipeYield, Capture, infer_pipeline, sentinel::listen, Edge, HeldEdge, Effect::ArmEdge, Event::Wrote]
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
`resolve_pipeline` (`pipeline/resolve.rs`) settles the terminal plan and calls
`resolve_launch` per stage — the single launch decision, reading the head's
resolution, its redirects, and whether a `!{…}` audit captures bytes:

- `Direct(ExternalStage)` — an external command or bundled tool spawned
  straight into the group as its own process, no ral in front of it;
- `Thread` — the stage's ral computation evaluated on its own OS thread, over
  a cloned `Shell`. An external head becomes a `Thread` stage too whenever a
  redirect or a byte-capturing audit rules `Direct` out; the external is then
  spawned from inside that thread instead.

No route is consulted — nor could one be, since the checked IR carries none:
a stage's classification cannot depend on where its payload lives, because the
choice must be observationally transparent. The node's own `PipeYield` — the
syntax the checker wrote in place of the last stage's route — never passes
through resolve at all: it comes committed in the checked IR and rides on
`PipeNode` itself. Launch consumes the frozen decisions; it does not re-derive
a transport mode. There is no
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

**The final-value bit is derived from position alone**, `i + 1 == n`; every
other stage is `is_last = false`. A `Thread` stage carries its value home on
its own `StageObservation`, and the node's `PipeYield` — held by `PipeNode`,
not by the plan — decides at `finish` whether that value or `Unit` is the
pipeline's.

**A non-final stage cannot observe its reader's death by EPIPE.** The parent
holds a duplicate of each interior edge's read end — `route::HeldEdge { edge,
reader }` — until that edge's writer stage is killed or reaped, so no interior
edge ever delivers a broken-pipe signal or a write error to the stage that
writes it: a dead edge is felt only through the mechanism below, at a write,
never through the OS's own broken-pipe path. `crate::io::Edge` is the edge's
own fate (`is_dead`/`mark_dead`), shared by the writer and the `HeldEdge` that
watches it. `CollectState::file` emits `Effect::ArmEdge(ix)` when a reader
stage is filed and its writer still runs: `StageHandle::arm` marks the edge
dead — before the sentinel snapshots what is pending, so a write completing
between the two is caught by the sink's own post-check rather than lost — and
starts `pipeline/sentinel.rs::listen(reader, slot)` on the `HeldEdge`'s read end
(`reader` moves out of the `HeldEdge`; it is `None` for as long as the
sentinel holds it), whatever kind the writer is. A `Thread` stage's
`Sink::Pipe { writer, wake, edge }` checks the same edge itself after each
chunk it lands, and raises `DeadEdge` — read back by `Shell::write_sink` as
`Error::cancelled(ReaderGone)` — so the break lands at the write, exactly;
the sentinel hears the same chunk and its redundant `KillStage` finds a stage
already unwinding. An external stage's write ral cannot see
directly, so the sentinel is what hears it: it reads whatever bytes are
already pending — owed to a reader that left, and discarded — and the first
byte written after them is `Event::Wrote(ix, reader)`, handing the read end
back. `yes | !{ return 5 }` terminates this way: `yes`'s own next write finds
the edge dead. Neither endpoint of a pipe promises traffic, so a producer
with nothing left to write for is the ordinary case, not an error path.

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
keeps, stdout a pipe the parent polls, `SIGTSTP`/`SIGTTIN`/`SIGTTOU` all set
`SIG_IGN` so the anchor essentially never stops
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]) — unlike the
rest of the group, which the kernel can still stop directly. Every termination signal
(`SIGINT`/`SIGTERM`/`SIGHUP`/`SIGQUIT`) is caught rather than obeyed: the
handler writes the signal number to stdout and the anchor lives on. That
immunity is what makes the group addressable from the moment `prepare`
returns, rather than from a first real stage spawn: the anchor is a genuine
member already, so the pgid is never child-less.
`PipelineBuild::new` claims the foreground before any stage runs —
`claim_foreground` (`group.rs`) is still gated on a borrowed `&TerminalLease`.
With the terminal settled before user code exists, no start-gate frame is
needed.

**The anchor is the group's witness for a signal — one dedicated thread; its
own stop is never witnessed at all, resumed by the reaper like any other
watched pid's.** The shell is not a member of the pgid, so while the terminal
belongs to the group the shell never receives the Ctrl-C or Ctrl-Z the tty
delivers to it. With an external in the group a Ctrl-C would at least kill
that external and the pipeline would drain; with none — `!{ a } | !{ b }` at
the prompt — the anchor is the only process the kernel can reach, and it
swallows the signal. `PipelineGroup::prepare` spawns the anchor and starts its
witness in one act, so no signal it swallows can land before someone is
reading for it: one dedicated thread blocks to EOF on the anchor's stdout —
one byte per swallowed signal, sending `Event::Witnessed(cause)`, `SIGINT`
mapping to `Interrupt`, `SIGQUIT` to `RootAbort`, the rest to `Terminate`, the
cause the shell's own handler for that signal would apply — and the anchor's
own `ChildHandle` goes to the reaper as a `Watch` whose posting closure maps
its exit to `Event::Cancelled` (a signal death to the cause its number names,
anything else to `Terminate`). ral does not
suspend ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]): the
anchor's own rare stop is answered with `SIGCONT` by the reaper itself,
inside its own scan, and never reaches this channel at all — no anchor
waiter thread exists to see one. A death, however it left, ends the loop; the
anchor is the group's join target, so any death cancels the pipeline, expected
(`AnchorProcess::finish`'s own teardown) or not.

**The two variants say who delivered.** `Witnessed` is the kernel's own
delivery — the tty gave that signal to every member of the pgid before the
anchor reported it — so teardown cancels the thread stages and waits out the
grace but sends no second copy. `Cancelled` is a cause nothing in the group
has yet been told about (the mooring's scope, the anchor's own death), so
teardown is what delivers it. Either way the event is applied to the pipeline
and not to the run's scope: `step` answers with
`Effect::CancelAll { cause, delivered }` and the pipeline's error break is
what ends the run. The report reader and the reaper's watch on the anchor's
own pid are two separate producers into the same channel, so the two can race:
whichever event arrives first is the one `step` answers.

A single-stage pipeline never reaches any of this: the machine's
`Pipeline` arm reduces it to its inner closure inline, and `PipeNode::launch`
never sees it.

**Every external spawned anywhere inside a stage joins the group.** A `Thread`
stage's `Io` carries `LaunchRole::PipelineStage(pgid)`, so any external it
spawns — at its own root, or nested arbitrarily deep — resolves
`PgidPolicy::Join(pgid)` and lands in the same group as a `Direct` stage would.
A `spawn` worker started inside a stage thread joins the group too, its `Io`
inherited; a stop reaching it is answered exactly as any other wait answers
one, so a detached worker is no more parked by Ctrl-Z than it is cancelled by
Ctrl-C ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).

**A stage thread can itself launch a pipeline, which joins rather than owns.**
The machine's `Pipeline` arm is `PipeNode`'s only caller and steps identically
inside a stage thread, so a stage whose body is itself a pipeline launches its
own nested stages. That nested `PipeNode` reads `shell.io.launch_role`: for
`PipelineStage(g)` it builds `PipelineGroup::joining(g)` instead of preparing
one — no anchor, no foreground claim, and `signal`/`kill` no-op, since only
the owning top-level group may act on the pgid. A stop reaching any member of
the nested group, direct or joined, is answered with `SIGCONT` by the one
process-wide reaper — the same rule wherever in the nesting the watched pid
sits, with nothing to forward upward and nothing for a joining collector to do
about a stop at all. A joining collector's own teardown therefore reaches its
live externals by pid, one signal each, since it has no pgid to address.

**A thread stage cuts itself at its own next write; an external is heard by
the sentinel and killed at the first byte after the discard.** Marking an
edge dead (`Effect::ArmEdge`) changes nothing for a stage not currently
writing — its own account continues regardless of writer kind. A `Thread`
stage's own write needs no cancel and no wake to be cut: its `Sink::Pipe`
checks `edge.is_dead()` after each chunk it lands — judged, not refused, so a
child's bytes carried through a pumped sink under `--audit` reach the edge
and are heard by the sentinel like any external's, and a write already in
flight when the edge dies finishes before it is judged — "a write not
complete when the edge dies does not complete." A
blocked write is never stranded, because something is always still reading
the other end — the reader stage, or the sentinel once it is gone — so the
write completes and is judged, never wedged waiting on a forced wake. The
resulting `Error::cancelled(ReaderGone)` reaches the stage's evaluation
exactly as any other error would, and its `Break` carries
`cancelled_by() == Some(ReaderGone)` (`Error::cancelled` is the one
constructor of a `Status::Cancelled(cause)`, read back by
`StageObservation::ended_by`). The fold forgives a thread stage on that fact
alone: a `ReaderGone` break is only ever a collector's own doing. A child the
stage thread spawned holding the edge (`!{ sh -c … } | true`) is heard by
the sentinel like any external; the `Effect::KillStage` that follows cancels
the stage's scope with `ReaderGone`, and its `RunningChild::wait` kills the
child by pid and mints the same break. A stage that never writes again after
its edge dies is never cut and keeps running: `sleep 1000 | true` waits for
the sleep.

An `External` stage has no write ral can see, so
`pipeline/sentinel.rs::listen(reader, ix, tx)` reads the held duplicate in its
place, discards whatever bytes are already pending — owed to a reader that
left — and turns the first byte written after them into
`Event::Wrote(ix, reader)`,
handing the read end back. Only then, not at the reader's own terminal event,
does the process die: `step` records `sent[ix] = ReaderGone`, joined by
`max`, and emits `Effect::KillStage(ix)`, which `StageHandle::cut` performs.
`grace_signal` offers no catchable signal for `ReaderGone`, so wherever a
reader-gone teardown runs — a stage's own `RunningChild::terminate`, a nested
pipeline's `cancel_all` — the kill is outright. The kill addresses the stage's
pid alone, so a descendant the stage forked outlives it and may still hold
the pipe a pump of that stage reads; an external ended for a dead write
therefore *detaches* its pumps rather than joining them
(`command::Pumps::settle(detach: bool)`, read from `sent[ix]` in the fold,
after the walk) — its remaining bytes are owed to nobody, and the join would
otherwise wait on the descendant. Forgiveness for an external reads the wait
status too, which a thread's `Break` does not carry on its own: `sent[ix] ==
Some(ReaderGone)` and the death is the kill's own
(`WaitOutcome::is_stage_kill`), as before. The stage's own external child (if
it has one) is torn down by pid inside that stage's own `RunningChild::wait`.
A nested pipeline's stages are cancelled transitively the same way, each
stage's own edge check or sentinel ending only that stage's own write. This
is also why the kill can never land on a stage that already finished:
`Effect::ArmEdge` and the `Effect::KillStage` it can lead to both require the
writer still `Some` (unsettled), so `!{ echo a; exit 3 } | head -1` stays
honest. Its audit fragment is folded either way: the verdict is the
collector's doing, what the stage observed still happened. A stage thread's
end is its own `Event::Returned`, sent as
its last act; a panic in the body is caught around the evaluation
(`catch_unwind(AssertUnwindSafe(..))`) and turned into the same `Returned`,
its payload becoming an `Error` carrying the stage's span. A `SettleOnDrop`
guard, constructed before the closure runs and moved into it, is each
index's own dead-man's switch: an unwind anywhere on that thread — even
before the closure body starts — drops the guard still armed, and the drop
itself posts the index's `Returned` (a generic "panicked" or "ended without
reporting" message, whichever `thread::panicking()` says). An external
stage has no waiter thread of its own left to carry a matching guard: the
reaper posts its exit and the collector's own fold is where anything about
it can go wrong, not a thread this collector must outlive.

**ral does not suspend, so Ctrl-Z has no story left to tell at the collector
at all — a stop never reaches its channel.** The kernel still stops what it
can stop directly on Unix — the anchor and every external in the group — but
none of that is this collector's problem any more: the process-wide reaper
([[map/core/io-process|io-process]]) answers every watched pid's stop with
`SIGCONT` inside its own scan and posts nothing for it, whether the pid is a
direct external stage's or the anchor's own. A stage thread's own interior
wait, for whatever child *it* spawned, still answers a stop inline itself,
through the same reaper — `RunningChild::wait` no longer runs a poll loop to
do it, just one blocking `recv` over a channel the reaper's watch feeds.
`Event` has no `Stopped` variant at all — there is nothing for `step` to
answer, because nothing a stop does is ever visible above the reaper that
resumed it ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).
This holds whether the group is the top-level owner or a nested `joining`
one: there is no role distinction left to make about a stop, since no party
above the reaper ever learns one occurred. `!{ a } | !{ b }`, a pipeline with
no external at all, no longer parks on Ctrl-Z either — the anchor's own watch
answers it and the pipeline runs on, exactly as `vim` does.

**Windows never had a foreground handoff to race and still has none.** There
is no `tcsetpgrp`, and the terminal plan never selects
`ForegroundExternalGroup`. The anchor still exists on Windows — it is what
keys the Job Object in `GROUPS` — but it witnesses nothing there, Windows
having no group-wide delivery to hear; every
external spawned inside a stage thread still joins the Job Object through the
same `PgidPolicy::Join` resolution, assigned at creation under the suspended
create → assign → resume path.

**Collection is one channel, a pure fold, and a thin interpreter.** Every
lifecycle edge — an external's raw exit, a stage settling, a cancel, a
sentinel's dead write — arrives as one [`Event`] on one channel; a stop is
not among them, answered and forgotten by the process-wide reaper before it
could become one. A stage
thread computes its own [`StageObservation`] and sends `Event::Returned` as
its last act; a direct external needs no dedicated waiter thread at all —
`ChildHandle::into_watch`'s own closure posts its raw
[`crate::process::WaitOutcome`] straight onto the collector's channel as
`Event::Ended(ix, outcome)`, the reaper being the one party that ever calls
`waitid` on it; the anchor's report reader sends `Event::Witnessed` for a
swallowed signal, and its own `Watch` posts `Event::Cancelled` for the
anchor's own death; a `watch_cancel` on the mooring's scope posts
`Event::Cancelled` the instant the scope's own cause is set. A stage's address
on that channel is its `Slot { ix, tx }` — handed out by `CollectState::slot`,
carried by every one of that stage's producers, and wrapped by the
`SettleOnDrop` a stage thread's closure owns.
Every index-owning producer — a stage thread's closure — carries its own
[`SettleOnDrop`], so its `Returned` reaches the channel by its own send or by
that guard's drop; the anchor's report reader and its own watch hold plain
sender clones and outlive every stage, but neither owns an index, so their
long life cannot make a stage's own end go missing. `CollectState` drops its
own clone once every stage is launched (`all_stages_launched`); a `recv` that
returns `None` regardless — every producer gone with some index never
settled — is therefore this collector's own defect, not a panic to recover,
and is treated as `Done`. `step` is pure over `CollectState`: it never
blocks, signals, or touches a process, only inspecting the observation
vector, and returns the [`Effect`]s a thin interpreter (`CollectState::run`)
performs — so the whole corner-case space is a transition table, testable by
feeding `step` a sequence of events and asserting the effects, no process,
sleep, or retry loop needed. `drive` is
`loop { for e in step(&mut st, rx.recv()?) { run(e) } }` — no interval, no
backoff, exact latency, zero idle CPU. `step` folds five kinds of event —
`Ended`, `Returned`, `Wrote`, `Cancelled`, `Witnessed` — a stop is not among
them at all: it is answered and forgotten by the reaper's own scan, never
reaching the channel
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).
`step`'s reader-gone rule is two effects, one per event that justifies it: on
a stage's own terminal event at index `ix`, if stage `ix - 1` still holds a
stage handle, emit `Effect::ArmEdge(ix - 1)`; on
`Event::Wrote(ix - 1, reader)` — the sentinel's report that `ix - 1`'s next
write found the edge dead — record `sent[ix - 1] = ReaderGone`, joined by
`max`, and emit `Effect::KillStage(ix - 1)`. Both check the writer is still
`Some` (unsettled) before acting, so a stage that already finished keeps its
outcome. The shell-touching finish this implies — reading `sent` back against
what happened, forgiving a `ReaderGone` kill — waits for
[`CollectState::fold`], the one place `&mut Shell` reaches an external's
settlement, since no reaper-posting closure or stage thread may hold one.
Each interior edge's held-open read end — handed back to the collector by
`Event::Wrote` once the sentinel confirms a dead write — drops once that
edge's writer's observation completes, which also releases any descendant of
that edge still blocked writing into it. Verdicts fold in launch order
regardless of settle order, so which stage the collector kills when never
changes which failure the fold reports.

**Teardown is kill-first.** A pipeline that ends before every stage has been
observed ends by one of two mechanisms, and both put the death of what this
collector launched before anything that blocks.
`CollectState::cancel_all(group, cause, delivered)` is the one that observes
on purpose — a cancel, a witnessed signal, the REPL's exit — and spells the
order out for both an owning and a joining group: (1) every live stage is
cancelled, recording the cause in `sent[ix]`, joined by `max`, as it goes; a
thread's scope is cancelled and the thread woken, while an external is left
untouched here, since a process hears a cancellation only as a signal;
(2) that signal is `grace_signal(cause)` — `Interrupt` → `SIGINT`,
`Explicit`/`Deadline`/`Terminate` → `SIGTERM`, `ReaderGone`/`RootAbort` →
none at all, straight to the kill — sent **once per process**: to the whole
pgid (then `SIGCONT`, since a stopped member cannot act on the first until it
runs) for an owning group, or per pid via `Watch::signal` for a joining one,
which has no pgid of its own. `delivered` skips this step outright: a
`Witnessed` cause is one the kernel already gave every member, and a second
copy would be a second interrupt; (3) a bounded grace of at most
`TEARDOWN_GRACE` (500 ms, shared with the standalone command's own
`terminate`) as a single blocking `recv_timeout` on the deadline, not a probe
loop, filing only a stage's own terminal event (`Ended`/`Returned`) it drains
— every other kind is moot once the whole group is already dying, and is
discarded; (4) the kill — `group.kill()` (`SIGKILL -pgid`) for an owning
group, or `kill_live_externals` (a pid-wise `Watch::kill` over every stage
still `Some`) for a joining one, which has no pgid to `SIGKILL` either; (5) a
further blocking drain, unbounded, for whatever the grace did not already
account for — a cancelled and killed member having nowhere left to block;
(6) the anchor last, in `Drop`, after every stage handle has gone: its report
reader is joined and its own `Watch` dropped, which reaps (the report
reader's read end closes only once it sees the anchor's own EOF, so it is
never dropped while the anchor could still `SIGPIPE` on a stray write into
it).

Because `grace_signal` is the one cause→signal table, and
`RunningChild::terminate` reads it too, a reader-gone teardown kills outright
at every nesting level: `!{ yes | cat } | head -1` ends the inner `yes`
without a catchable `SIGTERM` ever being attributed to it. What the collector
sent is read back in `fold`, through `CommandFailure::from_outcome(outcome,
sent)`, so a pipeline external torn down under `Explicit` or `Deadline`
reports the cause — "stopped because the call's time limit expired" — exactly
as a standalone external does
([[decisions/260905_one-delivery-path|one-delivery-path]]).

`CollectState::drop` is the other, and it covers every forced end that drops
rather than observes: a launch that failed part-way, an unwind between launch
and fold. When a stage is still unobserved —
a condition derived from what the collector already knows, not a flag a
caller must set — it kills the owned pgid, or, for a joining collector with no
pgid of its own, its own live externals by pid; either way the descendants
below it die before the handles join. Every stage observed is the ordinary
end, which a `spawn` worker that joined the pgid outlives.

Killing before joining is what makes the joins terminate: a stage's own kill
reaches its pid alone, and a pumped descendant that survived it would hold the
pump's pipe open forever. The two graces nest without adding — the group's
`SIGKILL` ends whatever the standalone command's own `terminate` grace is
waiting on for a stage's interior child. Descendants of a killed external are
the group owner's, as for any stage's own kill.
`PipelineBuild`'s and `PipeNode`'s field orders carry
the drop half: routes, then the collector, then the group. The collector's
kill must reach the pgid it named, and it is the live anchor that keeps that
pgid from being reused; the anchor is waited last precisely so nothing that
could still need its pgid joinable is waiting on it first.
Windows has no polite signal and no grace: the Job Object kill is the whole of
it, and `Drop` releases the group's `GROUPS` entry once the anchor is reaped.

**The terminal lease never moves for a stop.** A foreground pipeline that
takes `SIGTSTP` does not become a parked job, and there is nothing for the
terminal to hand back: the reaper answers the stop with `SIGCONT` inside its
own scan, invisibly to everything else, so the `ForegroundGuard` and the
pgid's `tcsetpgrp` ownership are never released and never need reacquiring
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]). Ctrl-Z on
`sleep 10 | cat` at the prompt is therefore invisible: the pipeline keeps the
foreground throughout, exactly as if the signal had not arrived.

See also [[design/pipelines|pipelines]],
[[internals/evaluator-machine|evaluator-machine]],
[[internals/capability-enforcement|capability-enforcement]],
[[decisions/260902_stages-are-threads|stages-are-threads]],
[[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]],
[[decisions/260905_the-cut-is-at-the-write|the-cut-is-at-the-write]],
[[decisions/260905_one-delivery-path|one-delivery-path]]; map
[[map/core/runtime|runtime]], [[map/core/io-process|io-process]].
`docs/SPEC.md` §7, §7.6, §11.6; RATIONALE §"The pipe is the operating system's".
