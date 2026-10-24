---
verified_at_commit: 3bf43d16fc35212e3a3b354a91ffa2fcac136724
verified_at_date: 2026-10-19
anchors: [PipeNode, PipeNode::launch, resolve_pipeline, resolve_launch, StageLaunch, StageLaunch::Direct, open_stage_routes, spawn_stage, launch_thread_stage, ThreadStage, StageHandle, file_external_end, file_thread_end, Slot, Event, Event::Heard, Event::Wrote, SettleOnDrop, Effect, Effect::ArmEdge, Effect::KillStage, Effect::CancelAll, Event::AnchorDied, step, StageObservation, StageEnd, CollectState, CollectState::fold, CollectState::run, CollectState::recv, CollectState::escalate, CollectState::cancel_all, CollectState::addresses, Address, kill_live, stronger, rank, grace_signal, PipelineGroup, PipelineGroup::prepare, PipelineGroup::joining, PipelineGroup::membership, Group, Membership, Membership::owes, AnchorProcess, ChildHandle, into_watch, Watch, Pumps::join_under, TerminalLoan, PipelineGroup::lend, PipelineGroup::end_anchor, TerminalLease, terminal_lease, Capture, infer_pipeline, sentinel::listen, Edge, HeldEdge]
---

# Pipeline execution: byte edges, one process group, threads and processes

[[design/pipelines|The design]] makes `|` a positional byte wire. Every interior
edge is an operating-system pipe from the left stage's stdout to the right
stage's stdin, alike for every pair; only the final stage may report a value.
The machine's `CompKind::Pipeline` arm
([[internals/evaluator-machine|the evaluator machine]]) reduces a single-stage
form to its inner closure and hands a multi-stage form to `PipeNode::run`, in
the same rule — no frame is pushed, since nothing
runs beneath the node — resolve, launch, join (drive then fold) are the
spine below.
A stage's own stack is empty by construction: **no stage runs in the parent, so
none can be in tail position, and no frame ever crosses into a stage** — only
⟨comp, captured env⟩ rides along, the environment being the pipeline node's own
lexical environment rather than `shell.env`; the stage thread's own `shell.env`
is the parent's session. Ordinary application and bind
compose values in the machine and do not enter this pipeline runtime.

**Resolve freezes a `StageLaunch` per stage from resolve-time facts.**
`resolve_pipeline` (`pipeline/resolve.rs`) settles the terminal plan and calls
`resolve_launch` per stage — the single launch decision, reading the head's
resolution, its redirects, and whether a `!{…}` audit captures bytes:

- `Direct { id, args }` — an external command or bundled tool spawned
  straight into the group as its own process, no ral in front of it; the
  resolved identity and its already-closed argument values are all launch
  needs, and only this arm evaluates argv here, a `Thread` stage evaluating
  its own inside the thread;
- `Thread` — the stage's ral computation evaluated on its own OS thread, over
  a cloned `Shell`. An external head becomes a `Thread` stage too whenever a
  redirect or a byte-capturing audit rules `Direct` out; the external is then
  spawned from inside that thread instead. The stage's redirects are then interpreted by `RedirectState`, the one
  interpreter a block and a standalone external share; a `Direct` stage has none,
  and `wire_stdio` wires its stage pipes without reading a redirect list.

No type is consulted: a stage's classification cannot depend on what it
returns, because the choice must be observationally transparent. A final stage
the checker wrapped in `Capture` (`let x = a | b`) is no `Exec`, so it is a
`Thread` stage whose stdin is the pipe and whose `cap` buffer takes what the
command writes. Launch consumes the frozen decisions; it does not re-derive
a transport mode. There is no
in-process pipeline fold and no typed value channel between stages.

**Every interior route is an operating-system byte pipe, allocated from stage
position alone.** `open_stage_routes` opens one `os_pipe` per interior edge and
zips the stdins `Parent : ins` against the stdouts `outs ++ [Parent]`, so a
stage's endpoints are decided by where it sits and by nothing else.
Nothing in that allocation reads a type, and nothing in it reads whether a
stage is a
thread or a process — a `StageRoute` is the same value either way, and
`Thread`/`Direct` differ only in how the route is wired into an `Io` versus a
child's stdio. A non-final stage's returned value is simply discarded, and no
returned value is ever serialised onto an edge
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

**No stage is told whether it is the last one.** Every stage reports whatever
it settled on its own `StageObservation`, and the value the pipeline keeps is
read out by position in the fold — stages fold in launch order, so the value
kept is the last `Ok`, which is the final stage's whenever that stage is `Ok`
and is unread when it is not, every other stage
contributing only its verdict and its audit.

**A non-final stage cannot observe its reader's death by EPIPE.** The parent
holds a duplicate of each interior edge's read end — `route::HeldEdge { edge,
reader }` — until that edge's writer stage is killed or reaped, so no interior
edge ever delivers a broken-pipe signal or a write error to the stage that
writes it while that stage is observed (a forced end, below, drops the hold
first): a dead edge is felt only through the mechanism below, at a write,
never through the OS's own broken-pipe path. `crate::io::Edge` is the edge's
own fate (`is_dead`/`mark_dead`), shared by the writer and the `HeldEdge` that
watches it. Before every wait, `CollectState::recv` sweeps the edges
(`sweep_edges`): every live stage whose reader is gone — filed, or never
launched (`abandoned`) — gets `StageHandle::listen`, whatever kind the writer
is. Outside a teardown it marks the edge dead first — before the sentinel
snapshots what is pending, so a write completing between the two is caught by
the sink's own post-check rather than lost — and then, once per edge
(`HeldEdge::listening`), starts `pipeline/sentinel.rs::listen(hold, slot)` on
the `HeldEdge`'s read end. The sentinel reads through a `Weak` of the hold's
`Arc<PipeReader>`, so the hold alone keeps the fd open: it closes when the
writer's filing drops the `HeldEdge`, or, if a read is in flight then, as that
read returns. A `Thread` stage's
`Sink::Pipe { writer, edge, stage }` checks the same edge itself after each
chunk it lands, and raises `WriteStopped` — read back by `Shell::write_sink`
as `Break::stopped(ReaderGone, Some(stage))`, a stop naming the writing
stage's own node — so the break lands at the write, exactly;
the sentinel hears the same chunk and its redundant `KillStage` finds a stage
already unwinding. An external stage's write ral cannot see
directly, so the sentinel is what hears it: it reads whatever bytes are
already pending — owed to a reader that left, and discarded — and the first
byte written after them is `Event::Wrote(ix)`, which carries nothing but the
index. `yes | !{ return 5 }` terminates this way: `yes`'s own next write finds
the edge dead. Neither endpoint of a pipe promises traffic, so a producer
with nothing left to write for is the ordinary case, not an error path.

**Bytes reach a stage thread through `Sink::Pipe` and a scoped
`SourceReader`.** A `Thread` stage's `Io` is built straight from its
`StageRoute`: `ByteOut::Downstream` becomes `Sink::Pipe { writer, edge, stage }`
as the stage's `stdout`, so a discarded statement's bytes go down the wire like
every other byte it writes. `stage` is the stage's *name*, for its reader-gone
stop; the scope a write polls is the writer's own current scope, handed in at
the write (`Sink::write_under`), so a cleanup's write is masked with its
shield.
A final stage has no edge, so `ByteOut::Parent` takes the parent's `stdout`:
under `!{ … | … }` that is a capture buffer, and the final stage writes into it
([[design/capture|capture]]).
On the way in, `ByteIn::Upstream` becomes
`Source::Reader(SourceReader::pipe(r).interruptible(scope))`,
the pipe reader beside the scope a blocked read samples between slices
(`POLL_SLICE`), the scope read first so an always-ready stream still polls.
A cause in force reads as EOF, never as an interrupted-read error, so
`std::io::copy` and friends cannot spin on it. A builtin's reader is re-scoped
to the current scope where it is obtained (`stdin_reader`), so a cleanup's
reads are masked with the cleanup. `Source::reader` never *takes* a source —
every consumer, a builtin, an external's stdin, a nested stage, receives a
`dup(2)` — so a stage's stdin outlives every command that reads it; the scope
is stripped before a child inherits the source, so it is never a child's
problem. `ByteIn::Parent` at
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
member already, so the pgid is never child-less. On Unix, the anchor writes
a zero readiness byte after installing all signal dispositions; `prepare`
waits for it before exposing the group. A key on the lent terminal therefore
cannot race the anchor's handler installation.
`PipeNode::run` lends the terminal as soon as the group exists, before
any edge is opened and so before any stage runs — `PipelineGroup::lend`
(`group.rs`) borrows the `&TerminalLease`, and only an owning group lends.
The `TerminalLoan` goes to the collector, and `LaunchCx.holds_terminal`
freezes whether it was made.
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
one byte per swallowed signal, sending `Event::Heard(signal)` raw, since only
the pipeline's `TerminalLoan` may read a signal as a key — and the anchor's
own `ChildHandle` goes to the reaper as a `Watch` whose posting closure maps
its every exit to `Event::AnchorDied`. ral does not
suspend ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]): the
anchor's own rare stop is answered with `SIGCONT` by the reaper itself,
inside its own scan, and never reaches this channel at all — no anchor
waiter thread exists to see one. A death, however it left, ends the loop. An
anchor that died on its own ends the pipeline (`Terminated`); one that died
inside a teardown died of the collector's own kill, and changes nothing — an
`Interrupted` teardown must not end `Terminated`.

**A heard key strikes; the scope decides.** `Heard` is the kernel's own
delivery — the tty gave that signal to every member of the pgid before the
anchor reported it — and only a lent terminal makes it a key: with no loan
(exarch, a capture, a stage thread, a worker's pipeline) it is inert, and so
is a signal no terminal sends, such as a stray SIGTERM. Hearing a key tears
nothing down: the loan strikes the run's node (`Mooring::home`) and records
`pressed`, and the teardown follows from the *current scope* the collector
reads at its next slice, which is what lets a cleanup's shield hold it off.
`pressed` is read only by `cancel_all`, to skip the group's grace signal the
tty already delivered. `Event::Cancelled` is minted by `recv` alone, for a
cause on the scope stronger than the teardown so far, so a teardown can
escalate; `step` answers it with `Effect::CancelAll { cause }`. The report
reader and the reaper's watch on the anchor's own pid are two separate
producers into the same channel, so the two can race: whichever event
arrives first is the one `step` answers.

**The anchor ends before the fold.** `join` is `drive`, `end_anchor`,
`fold`. The tty signals the whole group in one pass, so the anchor's SIGINT
is pending before any member can die; its handler writes the byte before
`read(0)` can return EOF; and `finish` drops the release end and joins the
reporter, which leaves only on the report pipe's EOF. After `end_anchor`,
then, every report is on the channel, even when every stage ended before
`drive` read one. `fold` hears them all out — each late key struck on the
node as it is heard — and settles each external stage under `sent` joined
with the cause on the current scope; a key the tty delivered is attributed
through that scope or not at all. The loan drops with the collector at the
end of `fold`, returning the terminal and striking nothing: the stages may all
have trapped the key and exited 0, and the strike already on the node is what
the run's own poll then reads. Ending the anchor first is safe: `drive`
returns only with no stage live, so nothing addresses the pgid again.

**A launch that fails part-way is rolled back, then joined.** The spawn
loop stops at the first stage it cannot launch — a head `vet` refuses, a
wiring or spawn that fails, a stop its poll reads — and the stages already
running are not dropped: `CollectState::roll_back` tears them down with
`cancel_all`, under the cause in force or else `Cancelled`, as for an
`exit` in a stage, and `join` follows as for any pipeline. So every stage
started is observed and settled, and every key the anchor reported while
the launch ran is struck on the run's node, where `try` cannot absorb it.
The launch's failure is the verdict, whatever the fold found: the fold's
own fallback is the rollback's stop, and what the started stages did
raced it.

A single-stage pipeline never reaches any of this: the machine's
`Pipeline` arm reduces it to its inner closure inline, and `PipeNode::run`
never sees it.

**Every external a stage's own evaluation spawns joins the group.** A `Thread`
stage's `Io` carries `Some(membership)` as its `stage` — a `Membership`
pairing the group's pgid with the stage thread's own cancel scope — so any
external it spawns — at its own root, or nested arbitrarily deep — resolves
`PgidPolicy::Join(pgid)` and lands in the same group as a `Direct` stage would.
**A process group is its owner's to signal.** Such an external's waiter holds
`Group::Joins(membership)`, and its `terminate` opens with a grace signal of
its own only for a cause `Membership::owes`: one stronger than the stage scope
holds, struck beneath the stage. Every cause the stage holds is one the owner
cancelled it with and delivers to the whole group, so the member hears it
once. An enveloped external is the exception that proves the rule: placed
`NewLeader` whatever was asked, it leads its payload's group, and
`Launch::spawn` returns that group — a spawn returns a pgid only for a group
the child leads — so its waiter holds `Group::Owns` and signals the payload
itself, which neither bwrap's pid nor the pipeline's group can reach.
A `spawn` worker is outside that evaluation: its `Io` is minted fresh
with `stage: None`, so the externals it launches lead groups of their
own and outlive the pipeline without ever touching its pgid. The member that
outlives a stage is instead a descendant the stage forked
(`sh -c 'sleep 30 &'`), which the group owns like any other. A stop reaching
either is answered exactly as any other wait answers one, so neither is parked
by Ctrl-Z ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).

**A stage thread can itself launch a pipeline, which joins rather than owns.**
The machine's `Pipeline` arm is `PipeNode`'s only caller and steps identically
inside a stage thread, so a stage whose body is itself a pipeline launches its
own nested stages. That nested `PipeNode` reads `shell.io.stage`: for
`Some(m)` it builds `PipelineGroup::joining(m)` instead of preparing
one — no anchor, no foreground claim, and a `Group::Joins(m)` where an owner
holds `Group::Owns(pgid)`, an anchor's presence being the whole of what
ownership means: only the owning top-level group may address the pgid, so the
collector below reads `Joins` and falls back to acting per pid. The nested
pipeline's own thread stages are handed that same membership
(`PipelineGroup::membership` returns a joiner's own, and pairs an owner's
leader with the stage's scope), so a member at any depth answers to the
outermost owner's stage scope. A stop reaching any member of
the nested group, direct or joined, is answered with `SIGCONT` by the one
process-wide reaper — the same rule wherever in the nesting the watched pid
sits, with nothing to forward upward and nothing for a joining collector to do
about a stop at all. A joining collector's own teardown therefore reaches its
live externals by pid, one signal each, and only for a cause its membership
owes: whatever the outer stage holds, the owner has already delivered to the
whole group. That is every cause it can hear — its cancel arrives on the very
scope its membership reads — so a joining collector signals nothing and waits
out the owner's delivery; `owes` earns its keep in a member's own waiter,
whose scope sits beneath the stage's and can be struck alone.

**A thread stage cuts itself at its own next write; an external is heard by
the sentinel and killed at the first byte after the discard.** Marking an
edge dead (`StageHandle::listen`) changes nothing for a stage not currently
writing — its own account continues regardless of writer kind. A `Thread`
stage's own write needs no cancel to be cut: its `Sink::Pipe`
checks `edge.is_dead()` after each chunk it lands — judged, not refused, so a
child's bytes carried through a pumped sink under `--audit` reach the edge
and are heard by the sentinel like any external's, and a write already in
flight when the edge dies finishes before it is judged — "a write not
complete when the edge dies does not complete." A
blocked write waits a slice at a time: each slice reads the writer's scope,
and a chunk that cannot land on a dead edge (the pipe full, nobody draining)
is the reader-gone stop too, judged at the slice. The resulting
`Break::stopped(ReaderGone, Some(stage))` is an escape, so no `try` in the
stage sees it; the stage's `guard` cleanups run, shielded, on the way out. The
fold forgives a thread stage **by name**: every stop naming the stage's own
node is the collector's own doing — its reader-gone stop, or the teardown it
ran — and is consumed (`StageObservation::names`, `forgiven`); a stop naming
any other node is forwarded, and so is a cleanup that halted on anything
else, which becomes the verdict. A reader-gone stop naming *any* node ranks
under everything in the fold (`rank` 0): its owner is a collector above,
which will consume it. A child the stage thread spawned holding the edge
(`!{ sh -c … } | true`) is heard by the sentinel like any external; the
`Effect::KillStage` that follows writes `ReaderGone` on the stage's node, and
its `RunningChild::wait` reads it at the next slice, kills the child by pid
and mints the same stop. Until that slice the child writes on into the
sentinel's drain; in a shielded cleanup the stop is masked for the grace, and
the child runs to completion. A stage that never writes again after
its edge dies is never cut and keeps running: `sleep 1000 | true` waits for
the sleep.

An `External` stage has no write ral can see, so
`pipeline/sentinel.rs::listen(hold, slot)` reads the held duplicate in its
place, discards whatever bytes are already pending — owed to a reader that
left — turns the first byte written after them into
`Event::Wrote(ix)`, which names the stage and nothing else, and then drains
the edge for as long as it is held, so no writer ever blocks on it. Only then, not at the reader's own terminal event,
does the process die: `step` emits `Effect::KillStage(ix)`, and
`StageHandle::cut` performs it — the kill itself, and the `ReaderGone` it
records on that handle's own `sent`, joined by `max` with whatever cause the
collector had already sent it.
`grace_signal` offers no catchable signal for `ReaderGone`, so wherever a
reader-gone teardown runs — a stage's own `RunningChild::terminate`, a nested
pipeline's `cancel_all` — the kill is outright. The kill addresses the stage's
pid alone, so a descendant the stage forked outlives it and may still hold
the pipe a pump of that stage reads; an external ended for a dead write
therefore *detaches* its pumps rather than joining them
(`command::Pumps::join_under`, skipped in `StageEnd::settle` from the
`sent` the filed handle carried onto its `StageEnd`) — its remaining bytes are
owed to nobody, and the join would otherwise wait on the descendant. Every
other external settlement joins its pumps a slice at a time and, on a cause in
force, kills an owned group once, so a survivor holding the pipe cannot hold
the run past the stop. A direct external has no node; the collector's own
record of what it sent (`sent`) is its name, and a death by what was sent is
consumed as a thread's is, by the collector reading `sent`. The stage's own external child (if
it has one) is torn down by pid inside that stage's own `RunningChild::wait`.
A nested pipeline's stages are cancelled transitively the same way, each
stage's own edge check or sentinel ending only that stage's own write. This
is also why the kill can never land on a stage that already finished:
the sweep and the `Effect::KillStage` it can lead to both require the
writer still `Some` (unsettled), so `!{ echo a; exit 3 } | head -1` stays
honest. Its audit fragment is folded either way: the verdict is the
collector's doing, what the stage observed still happened. A stage thread's
end is its own `Event::Returned`, sent as
its last act; a panic in the body is caught at the stage's base
(`ThreadEnd::of`) and reported as `ThreadEnd::Panicked`. A panic is a defect,
not a signal: the collector tears the pipeline down with `Aborted`, granting no
grace, and `fold` re-raises it on the run thread (`resume_unwind`), where the
top restores its checkpoint. A `SettleOnDrop`
guard, constructed before the closure runs and moved into it, is each
index's own dead-man's switch: an unwind anywhere on that thread — even
before the closure body starts — drops the guard still armed, and the drop
itself posts the index's `Returned` ("ral pipeline stage ended without
reporting"). An external
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
through the same reaper — `RunningChild::wait` receives a slice at a time
over a channel the reaper's watch feeds, reading its scope between slices.
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
having no group-wide delivery to hear, and it swallows every console event,
the group's own Ctrl-Break grace included; every
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
`waitid` on it; the anchor's report reader sends `Event::Heard` for a
swallowed signal, and its own `Watch` posts `Event::AnchorDied` for the
anchor's own death. The scope is no producer: `CollectState::recv` reads the
launching computation's current scope before each receive and receives at
most a slice (`POLL_SLICE`), minting `Event::Cancelled` itself for a cause
stronger than the teardown so far. A stage's address
on that channel is its `Slot { ix, tx }`, minted by the spawn loop, `launch`,
carried by every one of that stage's producers, and wrapped by the
`SettleOnDrop` a stage thread's closure owns.
Every index-owning producer — a stage thread's closure — carries its own
[`SettleOnDrop`], so its `Returned` reaches the channel by its own send or by
that guard's drop; the anchor's report reader and its own watch hold plain
sender clones and outlive every stage, but neither owns an index, so their
long life cannot make a stage's own end go missing. The collector keeps no
sender at all: the sending half is `PipeNode::run`'s, cloned into each `Slot`
and dropped once the stages are launched. A `recv` that returns `None` with a
stage still live — every producer of that index gone without settling it — is
this collector's own defect, not a panic to recover: `drive` returns, and the
unobserved stage is `Drop`'s to kill. A stage the fold then finds never
observed folds as a failure — "ral pipeline stage vanished without reporting" —
never silently as an absence, so no defect can leave the pipeline reporting
success.
`step` is pure over `CollectState`: it never blocks, and it performs no kill,
signal, or cancel — those are the [`Effect`] a thin interpreter
(`CollectState::run`) performs, and one event justifies at most one of them, so
`step` answers with an `Option<Effect>`. Filing an observation is not one:
reaping an already-exited pid and reclaiming a thread already returning is
bookkeeping this collector owns. So the whole corner-case space is a transition
table, testable by feeding `step` a sequence of events and asserting the
effect, no process, sleep, or retry loop needed. Completion is neither an
effect nor a flag but the state itself — the pipeline is finished exactly when
no stage handle is left — so `drive` is `while live() { recv; step; run }`, the
receive sliced so the scope is sampled at least every slice. `step` folds six
kinds of event —
`Ended`, `Returned`, `Wrote`, `Cancelled`, `AnchorDied`, `Heard` — a process
stop is not among
them at all: it is answered and forgotten by the reaper's own scan, never
reaching the channel
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).
The reader-gone rule has two halves. The arm is derived from state, not
returned by `step`: the sweep before every wait arms each live writer whose
reader is filed or never launched, so no event that files a reader, an escape
included, can drop it. The kill is `step`'s: on `Event::Wrote(ix - 1)` — the
sentinel's report that `ix - 1`'s next write found the edge dead — it emits
`Effect::KillStage(ix - 1)`, the `ReaderGone` being recorded by the `cut`
that effect stands for, on the handle's own `sent`. Both require the writer
still `Some` (unsettled), so a stage that already finished keeps its
outcome. The shell-touching finish this implies — reading a filed stage's
`sent` back against what happened, forgiving a `ReaderGone` kill — waits for
[`CollectState::fold`], the one place `&mut Shell` reaches an external's
settlement, since no reaper-posting closure or stage thread may hold one; the
verdicts it ranks join by `stronger` over `rank`: a reader-gone stop naming
any node contributes nothing, then `Error < Exit < Stopped c`, stops ordered
by cause, ties going to the earlier stage. **A stage's escape tears the
pipeline down at once**: a filed `Returned` carrying an `exit`, or a stop the
stage did not get from this collector, answers `Effect::CancelAll` with its
cause (`Cancelled` for an `exit`), the siblings having no use left — their
stops then name their own nodes and are consumed, so the escape is the
verdict: `!{ exit 3 } | !{ /bin/sleep 100 }` is `exit 3` at once. With no
panic and no stage halted, the teardown's own cause is the verdict, the
collector's closing observation, so a teardown every stage survived as a
clean EOF still ends the pipeline stopped.
Each interior edge's held-open read end — an `Arc` the `HeldEdge` alone
owns, any sentinel reading through a `Weak` — closes once that edge's
writer's observation completes (at the sentinel's next read, if one is in
flight), which also releases any descendant of that edge still writing into
it. Verdicts fold in launch order
regardless of settle order, so which stage the collector kills when never
changes which failure the fold reports.

**Teardown is kill-first, bar a cleanup's grace.** A pipeline that ends
before every stage has been observed ends by one of two mechanisms, and both
put the death of what this collector launched before anything that blocks
indefinitely. `CollectState::cancel_all(cause)` is the one that observes on
purpose — a stronger cause on the scope, a stage's escape or panic, the
anchor's death, a launch that failed part-way — and spells the order out for
both an owning and a joining
group, its own `Group` saying which of the two it is: (1) `escalate` records
the cause as `teardown` and writes it on every live stage in one sweep, so
their cleanups share one strike — `StageHandle::cancel` recording it on that
handle's own `sent`, and for a thread writing it on the stage's node, the
collector being that node's one writer; an external is left untouched here,
since a process hears a cancellation only as a signal; a cause `teardown`
already covers returns at once; (2) that signal is `grace_signal(cause)` —
`Interrupted` → `SIGINT`, `Cancelled`/`TimedOut`/`Terminated` → `SIGTERM`,
`ReaderGone`/`Aborted` →
none at all, straight to the kill — sent **once per process**, a thread
stage's member adding no copy of its own, to every `Address` the
collector enumerates (`CollectState::addresses`): the whole pgid (then
`SIGCONT`, since a stopped member cannot act on the first until it runs) for
`Group::Owns`, or per pid via `Watch::signal` for a joining
collector, which has no pgid of its own, and only when `Group::owes` the
cause: one the outer stage holds is the owner's, already sent to every
member — and, beside either, each
confined stage's *envelope*, the payload group bwrap's `--new-session` put in
a session of its own, which `Launch::spawn` learned from the `--info-fd` and
`ExternalStage::envelope` kept. A key the tty already delivered
(`pressed() >= cause`) leaves out the pipeline's own addresses, never the
envelopes: the kernel already gave every member of the foreground group that
signal, and a second copy would be a second interrupt — but an envelope's
session was never in that group; (3) a bounded grace of at most
`TEARDOWN_GRACE` (500 ms, shared with the standalone command's own
`terminate`), drained a slice at a time, filing every stage's own terminal
event and escalating as it goes — a filed panic to `Aborted`, a stronger cause
on the scope to itself — a kill for a dead write being moot once the group is
dying, and the sweep before each wait setting the sentinel unmarked, since a
teardown cuts nothing at an edge: a shielded cleanup upstream of an edge
whose reader has gone, by the teardown, by the escape that began it, or by
never launching, neither blocks on it nor is cut — and then,
unless the teardown is `Aborted`, a wait for live *thread* stages bounded
by the run's `cleanup_grace`, since a thread stage may be in a shielded
cleanup whose children share the group; no shield masks `Aborted`, so under it
that wait would end on its own; (4) the kill — `kill_live`, the same addresses once more: `SIGKILL
-pgid` for an owned group, else a pid-wise `Watch::kill` over every stage still
`Some`, a joining collector having no pgid to `SIGKILL` either, and every
envelope by its own pgid; (5) a
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
sent is read back in `fold`, through `WaitOutcome::classify`, so a
pipeline external torn down for its own stage is consumed, and the teardown's
cause — `timed out`, status 124, for a wall — is the pipeline's stop, exactly
as a standalone external's
([[decisions/260905_one-delivery-path|one-delivery-path]],
[[internals/cancellation|cancellation]]).

`CollectState::drop` is the other, and it covers the one end that drops rather
than observes: an unwind between launch and fold. When a stage is still
unobserved —
a condition derived from what the collector already knows, not a flag a
caller must set — it writes `Terminated` on every live thread stage in one
sweep, so the joins in the stages' own drops share one grace, then kills the
owned pgid, or, for a joining collector with no
pgid of its own, its own live externals by pid; either way the descendants
below it die before the handles join. Under a guest
jail `kill_live` also kills each live external's `JailCgroup` before any
drain or handle wait, and each `JailCgroup`, live or filed, is finished as
the collector drops it, so a member that `setsid()`'d out of the group dies
too. Every stage observed is the ordinary
end, which a descendant a stage forked outlives.

Killing before joining is what makes the joins terminate: a stage's own kill
reaches its pid alone, and a pumped descendant that survived it would hold the
pump's pipe open forever. The two graces nest without adding — the group's
`SIGKILL` ends whatever the standalone command's own `terminate` grace is
waiting on for a stage's interior child. Descendants of a killed external are
the group owner's, as for any stage's own kill. A handle drops its hold on its
outbound edge before it joins its stage (`StageHandle` declares `held` before
`kind`), so a thread stage blocked writing to that edge fails on the broken
pipe once its reader's own end closes, instead of holding the join for its
grace; a forced end has no verdict to protect.
`PipeNode`'s field order carries the drop half — collector, then group. The collector's
kill must reach the pgid it named, and it is the live anchor that keeps that
pgid from being reused; the anchor is waited last precisely so nothing that
could still need its pgid joinable is waiting on it first.
On Windows the grace is Ctrl-Break to every member of an owned group
(`break_pipeline_group`: one event per pid on the job's member list, nested
members included, each stage being its own console-group root), which the
anchor swallows; a pid gets no grace, the Job Object kill following alone, and
`Drop` releases the group's `GROUPS` entry once the anchor is reaped.

**The terminal lease never moves for a stop.** A foreground pipeline that
takes `SIGTSTP` does not become a parked job, and there is nothing for the
terminal to hand back: the reaper answers the stop with `SIGCONT` inside its
own scan, invisibly to everything else, so the `TerminalLoan` and the
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
[[decisions/260905_one-delivery-path|one-delivery-path]],
[[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]],
[[decisions/261016_a-cleanup-is-shielded-within-its-grace|a-cleanup-is-shielded-within-its-grace]]; map
[[map/core/runtime|runtime]], [[map/core/io-process|io-process]].
`docs/SPEC.md` §7, §7.6, §11.6; RATIONALE §"The pipe is the operating system's".
