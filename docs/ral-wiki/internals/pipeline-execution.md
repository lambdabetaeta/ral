---
verified_at_commit: f26144e0
verified_at_date: 2026-11-09
anchors: [StartFailure, StartFailure::settle, PipeNode, PipeNode::run, Prepared::start, Prepared::run, Connection, PendingDrains, resolve_pipeline, resolve_launch, StageLaunch, StageLaunch::Direct, open_stage_routes, spawn_stage, launch_thread_stage, ThreadStage, StageHandle, file_external_end, file_thread_end, Slot, Event, Event::Heard, Event::Wrote, SettleOnDrop, Effect, Effect::KillStage, Effect::CancelAll, Event::AnchorDied, step, StageObservation, End, StageEnd, CollectState, CollectState::fold, CollectState::run, CollectState::recv, CollectState::escalate, CollectState::cancel_all, CollectState::addresses, Address, kill_live, stronger, rank, grace_signal, PipelineGroup, PipelineGroup::prepare, PipelineGroup::joining, PipelineGroup::membership, Group, Membership, Membership::owes, AnchorProcess, ChildHandle, into_watch, Watch, Drains::join_under, TerminalHold, PreparedTerminalLoan, PreparedTerminalLoan::lend, GroupOwner, Shell::group_owner, Placement, Placement::command, Placement::input, TerminalInput, Source::offered, CollectState::vacate, PipelineGroup::end_anchor, TerminalLease, TerminalAccess::reach, Capture, infer_pipeline, sentinel::listen, Edge, HeldEdge, Started::collect, Started::register, CollectState::lone, CollectState::settle_lone, drained, AnchorProcess::end, Group::owned, Group::owes, Job, Job::drain, JailCgroup::kill]
---

# Pipeline execution: byte edges, one process group, threads and processes

[[design/pipelines|The design]] makes `|` a positional byte wire. Every interior
edge is an operating-system pipe from the left stage's stdout to the right
stage's stdin, alike for every pair; only the final stage may report a value.
The machine's `CompKind::Pipeline` arm
([[internals/evaluator-machine|the evaluator machine]]) hands every form, a
single stage included, to `PipeNode::run` in one rule — no frame is pushed,
since nothing runs beneath the node — and resolve, launch, join (drive then
fold) are the spine below.
A stage's own stack is empty by construction: **no stage runs in the parent, so
none can be in tail position, and no frame ever crosses into a stage** — only
⟨comp, captured env⟩ rides along, the environment being the pipeline node's own
lexical environment rather than `shell.env`; the stage thread's own `shell.env`
is the parent's session. Ordinary application and bind
compose values in the machine and do not enter this pipeline runtime.

**Resolve freezes a `StageLaunch` per stage from resolve-time facts.**
`resolve_pipeline` (`pipeline/resolve.rs`) calls `resolve_launch` per stage —
the single launch decision, reading the head's
resolution, its redirects, and whether a `!{…}` audit captures bytes:

- `Direct { command, args }` — an external command or bundled tool spawned
  straight into the group as its own process, no ral in front of it; the
  resolved head and its already-closed argument values are all launch
  needs, and only this arm evaluates argv here, a `Thread` stage evaluating
  its own inside the thread;
- `Thread` — the stage's ral computation evaluated on its own OS thread, over
  a cloned `Shell`. An external head becomes a `Thread` stage too whenever a
  redirect or a byte-capturing audit rules `Direct` out; the external is then
  spawned from inside that thread instead. The stage's redirects are then interpreted by `RedirectState`, the one
  interpreter a block and a standalone external share; a `Direct` stage has none,
  and `Prepared::start` connects its stage pipes without reading a
  redirect list.

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
chunk it lands, and returns `WriteFault::Stopped(ReaderGone, stage)` —
`Shell::write_sink` turns it into `Break::stopped(ReaderGone, Some(stage))`, a
stop naming the writing stage's own node — so the break lands at the write, exactly;
the sentinel hears the same chunk and its redundant `KillStage` finds a stage
already unwinding. An external stage's write ral cannot see
directly, so the sentinel is what hears it: it reads whatever bytes are
already pending — owed to a reader that left, and discarded — and the first
byte written after them is `Event::Wrote(ix)`, which carries nothing but the
index. `yes | !{ return 5 }` terminates this way: `yes`'s own next write finds
the edge dead. Neither endpoint of a pipe promises traffic, so a producer
with nothing left to write for is the ordinary case, not an error path.

**Bytes reach a stage thread through `Sink::Pipe` and a scoped
`SourceReader`.** Each launch consumes its pipeline-local `ByteOut`.
`ByteOut::Downstream { writer, held }` transfers the write end to the stage
and returns the hold to the collector's stage handle. A
`Thread` stage writes the edge as `Sink::Pipe { writer, edge, stage }`, its
`stdout`, so a discarded statement's bytes go down the wire like every other
byte it writes. `stage` is the stage's *name*, for its reader-gone stop; the
scope a write polls is the writer's own current scope, handed in at the write
(`Sink::write_under`), so a cleanup's write is masked with its shield. A
`Direct` stage has no node to name, so it never holds a `Sink::Pipe`: its
child is handed the write end whole through `Connection::to_pipe`, and the
sentinel, not the fate, hears its writes. A connection borrows parent streams
only after admission; every output drain owns its reader before spawn, and
registration starts it without inspecting the child's stream fields.
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
problem. `ByteIn::Parent` at stage 0 resolves against the shell's own stdin
through `Source::offered(LaunchCx.input)`. The pipeline selects this
`TerminalInput` once at its owner boundary: a nested pipeline passes its
owner's source through; an owning pipeline offers the terminal only after a
successful loan. Without one, `Source::Terminal` becomes `Source::Empty`;
pipes and files pass through unchanged. A lone command uses the same
interpretation with `Placement::input`.
`Source::Terminal` means a terminal: a non-tty fd 0 became a `Source::Reader` once, at session
start (the `Runtime` holds its one close-on-exec dup, and each run's
inherited stdin is minted from it). A thread stage's tty
passes as `Source::Terminal`: its externals inherit it and `stdin_reader`
refuses it, since a thread in the shell process cannot read a tty whose
foreground is the externals' group.

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
`PipeNode::run` matches `Shell::group_owner` once. A nested pipeline joins
its stage's membership with readable input and an empty hold. An owning
pipeline consumes any `TerminalPermit` with `PreparedTerminalLoan::capture`
**before the anchor exists**. `PipelineGroup::prepare` lends only after the
zero byte and witness readiness, to the anchor's fresh pgid in ral's session,
and returns `(group, TerminalHold, TerminalInput)`:

- success retains the opaque hold and selects `Readable`;
- no preparation selects an empty hold and `Withheld`;
- refusal returns the preparation intact, then explicitly drops it and selects
  an empty hold and `Withheld`. Only the anchor exists: it reads its release
  pipe and reports signals, never changes modes. No stage needs teardown
  before this restoration; this is not a rule for arbitrary children.

The collector owns the hold. `LaunchCx.input` stores the chosen input for
both direct and thread stages, without inspecting the hold or recomputing
permission at each launch. `Pgid` proves positivity only; the two handoff
callers establish freshness and session membership by their spawn paths.
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
the pipeline's opaque `TerminalHold` may attribute a signal as a key — and
the anchor's
own `ChildHandle` goes to the reaper as a `Watch` whose posting closure maps
its every exit to `Event::AnchorDied`. The
anchor's own rare stop is the reaper's, as any watched pid's is (below). A
death, however it left, ends the loop. An
anchor that died on its own ends the pipeline (`Terminated`); one that died
inside a teardown died of the collector's own kill, and changes nothing — an
`Interrupted` teardown must not end `Terminated`.

**A heard key strikes; the scope decides.** `Heard` is the kernel's own
delivery — the tty gave that signal to every member of the pgid before the
anchor reported it — and only a lent terminal makes it a key: with no loan
(exarch, a capture without explicit access, a stage thread, a worker's pipeline) it is inert, and so
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
`read(0)` can return EOF; and `end` drops the release end and joins the
reporter, which leaves only on the report pipe's EOF. After `end_anchor`,
then, every report is on the channel, even when every stage ended before
`drive` read one. `fold` hears them all out — each late key struck on the
node as it is heard — and settles each external stage under `sent` joined
with the cause on the current scope; a key the tty delivered is attributed
through that scope or not at all. The loan is reclaimed at the
start of `fold` (`CollectState::vacate`), returning the terminal before any pump
is joined and striking nothing: the stages may all have trapped the key and
exited 0, and the strike already on the node is what the run's own poll then
reads. Ending the anchor first is safe: it only ends
the anchor's witness, and the ended anchor stays a zombie, unreaped until its
`PipelineGroup` drops, so the pgid stays pinned through every kill the fold's
pump joins make.

**A launch that fails part-way is rolled back, then joined.** The spawn loop
stops at the first stage it cannot launch — a head `Command::admit` or argument
validation refuses, a wiring or spawn that fails, a stop its poll reads — and the stages
already running are not dropped: `CollectState::roll_back` tears them down with
`cancel_all`, under the cause in force or else `Cancelled`, as for an `exit` in
a stage, and `join` follows as for any pipeline. So every stage started is
observed and settled, and every key the anchor reported while the launch ran is
struck on the run's node, where `try` cannot absorb it. The launch's failure is
the verdict, whatever the fold found: the fold's own fallback is the rollback's
stop, and what the started stages did raced it.

A standalone external enters no `PipeNode`: it supplies its own placement and
boundary streams, but shares the child execution machinery.

**The collector is every external's one waiter, and a standalone command is
a pipeline of one.** `Prepared` connects the streams and spawns for both
cases: `start` for a stage, `run` (which titles the terminal between the two)
for a standalone command. The pumps start before the spawn, so `Started::register`
(`pipeline/collect.rs`) only subscribes to the reaper, in one place; it addresses an envelope
separately only when the collector does not already own its payload's group.
A standalone command's `Started::collect`, beside it, takes the opaque
`Spawned.terminal` established by `Launch::spawn` and builds
`CollectState::lone` (slot 0, no thread stage, so no cleanup grace to grant). It runs the same `drive` as any pipeline, then `settle_lone` settles it
as a command ends. No other wait on an external exists: the
reaper's `Watch` posts its exit as `Event::Ended` and one `cancel_all` tears it
down, whether the child is a stage, a stage's own child, or a bare command.
**Where the child landed (`Group`, from the placement applied at spawn)
decides what the collector may address**, and the collector reads it from
nothing else ([[invariants/a-process-group-is-its-owners-to-signal|a-process-group-is-its-owners-to-signal]]).
The code facts behind each landing:

- An `Owns` child leads a group of one with no anchor. An enveloped child is
  stepped `NewLeader` by `Launch::spawn` whatever placement was requested.
  Its payload leads a separate session's group, returned as `Group::Enveloped`,
  which neither bwrap's pid nor the pipeline's group can reach. A requested
  `Tenant` becomes a restoration-only hold: readable stdin, no handoff, no
  key attribution.
- A `Joins` child belongs to a `Thread` stage's group. The stage's shell carries
  `Some(membership)` as its session's `stage`, a `Membership` pairing the
  group's pgid with the stage thread's own cancel scope, so any external it
  spawns, at its own root or nested arbitrarily deep, resolves
  `Placement::Join(membership)`. Its collector opens with a grace signal only
  for a cause `Membership::owes`, one stronger than the stage scope holds, struck
  beneath the stage.
- A `Shares` child sits in ral's own group (`Placement::Inherit`); `Group::owes`
  is total over the four arms.

A `spawn` worker is outside that evaluation: its session `stage` is
`None`, so the externals it launches lead groups of their
own and outlive the pipeline without ever touching its pgid. The member that
outlives a stage is instead a descendant the stage forked
(`sh -c 'sleep 30 &'`), which the group owns like any other.

**One pin holds a group's id until its last kill.** The pin is the process
whose pid is the group's id, kept as a zombie after it ends: the pipeline's
anchor (`PipelineGroup::Owns`, reaped by its `Watch` when `AnchorProcess` drops
with the group, after the fold), or a lone leader (`CollectState.pin`, an
`Arc<Watch>` shared with its stage). `StageHandle::file_external_end` reaps a
filed external through `Arc::try_unwrap` unless a pin still holds it, so the
zombie outlives its stage's end and a pgid that `kill(-pgid)` may still address
is never freed for reuse
([[invariants/a-group-id-is-held-until-its-last-kill|a-group-id-is-held-until-its-last-kill]]).
Behind an envelope the payload leads the group and the child is only its
monitor, so there is no pin once the monitor ends.

**A lone command settles in one order, and the terminal comes first.**
A successful `Prepared::start` hands `Started::collect` its opaque hold;
collection reaches `drive`, then `settle_lone`:

1. `drive` returns once the child's end is filed; on Windows `Job::drain` has
   first waited for every member of the job to leave (below);
2. `TerminalHold::reclaim` restores the terminal the instant its tenant is
   dead. A lent hold hears the child's own `death`, with no anchor to report
   a key, and strikes the run's node; an envelope guard restores modes without
   attributing a key;
3. `drained`: the guest jail's cgroup is finished first (its kill frees a
   straggler holding a pump's pipe), then the pumps are joined a slice at a
   time, killing an owned group once if a cause ends the join; a child cut for a
   dead write detaches them instead; it then reads the cause the child ended
   under;
4. the pin is dropped, no kill addressing the group any more;
5. `command::ending` reads the death against what was `sent` and the current
   scope, once.

The terminal never waits on a pump
([[invariants/the-terminal-returns-before-the-pumps|the-terminal-returns-before-the-pumps]]).
A pipeline's `fold` opens with `CollectState::vacate`, which consumes the hold
once no stage is live, then runs the same `drained` per external
(`StageEnd::settle`), its pin being the anchor, held to the group's drop.

**A failed start owns its drains until the caller can settle them.**
`Launch::spawn` owns child cleanup; `Prepared::start` returns `StartFailure`
with its reason and drains. A direct tenant's refused handoff may follow exec
and emitted bytes: it kills the owned group while the unreaped leader pins the
ID, kills/reaps the child, then restores the retained preparation and releases
the jail. A joined launch cannot kill its borrowed group: descendants holding
stderr must not prevent its enclosing owner from rolling back.

- `launch` and `spawn_stage` carry failed-start responsibility without joining.
- `PipeNode::finish` rolls back even if no stage registered, joins the pipeline
  and returns its terminal, then settles failed-start drains before returning
  the launch error. Only an owning node addresses its still-pinned group.
- A lone `Prepared::run` settles before propagating the reason.
- `StartFailure::settle` joins with an empty kill callback: no numeric ID is
  retained after reap. Dropping drains on unwind remains non-blocking.
- A joining nested node leaves group teardown to its parent. Inherited
  cancellation remains independently observable by the owning collector while
  the nested thread settles. Without cancellation, joined/shared descendants
  can delay settlement until EOF or external owner cancellation; a refusal
  does not grant authority to kill the borrowed group or cancel its owner.

**A stage thread can itself launch a pipeline, which joins rather than owns.**
The machine's `Pipeline` arm is `PipeNode`'s only caller and steps identically
inside a stage thread, so a stage whose body is itself a pipeline launches its
own nested stages. That nested `PipeNode` reads `shell.session.stage`: for
`Some(m)` it builds `PipelineGroup::joining(m)` instead of preparing
one — no anchor, no foreground claim, and a `Group::Joins(m)` where an owner
holds `Group::Owns(pgid)`, an anchor's presence being the whole of what
ownership means: only the owning top-level group may address the pgid, so the
collector below reads `Joins` and falls back to acting per pid. The nested
pipeline's own thread stages are handed that same membership
(`PipelineGroup::membership` returns a joiner's own, and pairs an owner's
leader with the stage's scope), so a member at any depth answers to the
outermost owner's stage scope. A joining collector's own teardown reaches its
live externals by pid, one signal each, and only for a cause its membership
owes: whatever the outer stage holds, the owner has already delivered to the
whole group. That is every cause it can hear — its cancel arrives on the very
scope its membership reads — so a joining collector signals nothing and waits
out the owner's delivery; `owes` earns its keep in a member's own collector,
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
fold forgives it by name (below). A child the stage thread spawned holding the edge
(`!{ sh -c … } | true`) is heard by the sentinel like any external; the
`Effect::KillStage` that follows writes `ReaderGone` on the stage's node, and
its own collector reads it at the next slice, kills the child by pid
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
reader-gone teardown runs — a stage's own child's collector, a nested
pipeline's, both `cancel_all` — the kill is outright. The kill addresses the stage's
pid alone, so a descendant the stage forked outlives it and may still hold
the pipe a pump of that stage reads; an external ended for a dead write
therefore *detaches* its pumps rather than joining them
(`io::child::Drains::join_under`, skipped in `StageEnd::settle` from the
`sent` the filed handle carried onto its `StageEnd`) — its remaining bytes are
owed to nobody, and the join would otherwise wait on the descendant. Every
other external settlement joins its pumps a slice at a time and, on a cause in
force, kills an owned group once, so a survivor holding the pipe cannot hold
the run past the stop. The stage's own external child (if
it has one) is torn down by pid by that child's own collector.
A nested pipeline's stages are cancelled transitively the same way, each
stage's own edge check or sentinel ending only that stage's own write. This
is also why the kill can never land on a stage that already finished:
the sweep and the `Effect::KillStage` it can lead to both require the
writer still `Some` (unsettled), so `!{ echo a; exit 3 } | head -1` stays
honest. Its audit fragment is folded either way: the verdict is the
collector's doing, what the stage observed still happened. A stage thread's
end is its own `Event::Returned`, sent as
its last act; a panic in the body is caught at the stage's base
(`Shell::base`, which also reads the stage's node once more after the body, so
a cut landing after its last poll is its ending) and reported as
`Ending::Panic`. `StageObservation.end` is
`End = Ending<Value, Error, Stop>`, shared with runs and workers
([[design/endings|endings]]). A panic is a defect,
not a signal: the collector tears the pipeline down with `Aborted`, granting no
grace, and `fold` re-raises it on the run thread (`resume_unwind`), where the
top restores its checkpoint and reports a `Panic` ending, status 101.
A `SettleOnDrop` guard, armed as the thread's first act, is each index's own
dead-man's switch: an unwind anywhere on that thread after it drops the guard
still armed, and the drop itself posts the index's `Returned` as
`End::unreported`, a panic the fold re-raises like any stage's. An external
stage has no waiter thread of its own left to carry a matching guard: the
reaper posts its exit and the collector's own fold is where anything about
it can go wrong, not a thread this collector must outlive.

**SIGCONT is the reaper's, and a stop never reaches the collector.** ral does
not suspend ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).
The kernel still stops what it can stop directly on Unix, the anchor and every
external in the group, but the process-wide reaper
([[map/core/io-process|io-process]]) answers the stop of every watched pid with
`SIGCONT` inside its own scan (`reaper/unix.rs`) and posts nothing for it.
`Event` therefore has no `Stopped` variant: nothing a stop does is visible
above the reaper, so `step` has nothing to answer, and nothing distinguishes a
direct stage's pid from the anchor's, an owning collector from a joining one,
or one depth of nesting from another. A stage thread's interior wait for its
own child follows the same rule through its own collector. `!{ a } | !{ b }`,
a pipeline with no external at all, therefore no longer parks on Ctrl-Z, and
Ctrl-Z on `sleep 10 | cat` is invisible: the terminal hold and the pgid's
`tcsetpgrp` ownership are never released. A teardown's own `SIGCONT`, which
follows a group's grace signal, is a different act, the collector's (below).

**Windows shares the console without transferring foreground ownership.**
Capture infallibly saves any available standard console modes; drop restores
those same console objects. Handoff succeeds without a syscall, and
`gesture` returns `None`: ral already hears console Ctrl-C directly, so a
child death reports no additional key. The anchor still exists on Windows — it is what
keys the Job Object in `GROUPS` — but it witnesses nothing there, Windows
having no group-wide delivery to hear, and it swallows every console event,
the group's own Ctrl-Break grace included; every
external spawned inside a stage thread still joins the Job Object through the
same `Placement::Join` resolution, assigned at creation under the suspended
create → assign → resume path.

**On Windows a group is one owned `Job` value.** The Job Object and its `GROUPS`
entry are held by exactly one `Job` (`pipeline/group.rs`): `PipelineGroup`'s for
an owning pipeline, `Started::collect`'s for a lone `Owns` child. Its `Drop` releases
both, killing whatever member is left; `Job::drain` waits, a slice at a time,
for every member to leave the job, a leader's descendants included, and kills
the job once if a cause is in force on the scope. `Started::collect` declares the job
before its collector, so an unwind kills before it releases, and drains it
between `drive` and `settle_lone`, so a lone command's settlement begins only
with its job empty. A pid address gets no Ctrl-Break: it is killed at once.

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
observed folds as `End::unreported`, re-raised as a panicked stage is, never
silently as an absence, so no defect can leave the pipeline reporting
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
`sent` back against what happened, forgiving what the collector caused — waits for
[`CollectState::fold`], the one place `&mut Shell` reaches an external's
settlement, since no reaper-posting closure or stage thread may hold one; the
verdicts it ranks join by `stronger` over `Break::rank`, the order a base's
closing poll ranks by too: a reader-gone stop naming any node contributes
nothing, then `Error < Exit < Stopped c`, stops ordered by cause, ties going to the earlier stage. **A stage's escape tears the
pipeline down at once**: a filed `Returned` carrying an `exit`, or a stop the
stage did not get from this collector, answers `Effect::CancelAll` with its
cause (`Cancelled` for an `exit`), the siblings having no use left — their
stops then name their own nodes and are consumed, so the escape is the
verdict: `!{ exit 3 } | !{ /bin/sleep 100 }` is `exit 3` at once.
Each interior edge's held-open read end — an `Arc` the `HeldEdge` alone
owns, any sentinel reading through a `Weak` — closes once that edge's
writer's observation completes (at the sentinel's next read, if one is in
flight), which also releases any descendant of that edge still writing into
it. Verdicts fold in launch order
regardless of settle order, so which stage the collector kills when never
changes which failure the fold reports.

**The collector forgives what it caused, and nothing else.** The fold
consumes a stop only when the collector's own act produced it.

- A thread stage's stop that names the stage's own node, whatever its cause
  (its reader-gone stop, or the teardown the collector ran), is forgiven:
  `StageObservation::names` selects it and `forgiven` makes it `Ok`, the audit
  kept. A stop naming any other node is forwarded, and so is a cleanup that
  halted on anything else, which becomes the verdict.
- An external's death is forgiven when it is a death by the cause the
  collector `sent` it (`Settlement::observe`, through `WaitOutcome::cancelled_by`).
  Any cause the collector sent is forgiven, not only `ReaderGone`; a direct
  external has no node, and `sent` is its name. A cause in force that the
  collector did not send is not: `drained` reads the cause as `sent` joined by
  `max` with the scope's, a `ReaderGone` in force counting for nothing, since
  it names a collector above.
- With no panic and no stage halted, the teardown's own cause is the verdict,
  so a teardown every stage survived as a clean EOF still ends the pipeline
  stopped. It names an owner only when the cause in force on the scope is the
  teardown's.

A reader-gone stop naming any node ranks 0 in the fold (`Break::rank`): its
owner is a collector above, which consumes it.

**Teardown is kill-first, bar a cleanup's grace.** The order is the
invariant's ([[invariants/a-teardown-kills-before-it-joins|a-teardown-kills-before-it-joins]]);
here is what `CollectState::cancel_all(cause)` reads to carry it out. It is the
teardown that observes on purpose: a stronger cause on the scope, a stage's
escape or panic, the anchor's death, a launch that failed part-way.

- `escalate` records the cause as `teardown` and writes it on every live stage
  in one sweep, so their cleanups share one strike: `StageHandle::cancel`
  records it on that handle's `sent`, and for a thread writes it on the stage's
  node, the collector being that node's one writer. An external is left
  untouched, since a process hears a cancellation only as a signal; a cause
  `teardown` already covers returns at once.
- The grace signal is `grace_signal(cause)`, the one cause-to-signal table
  ([[internals/cancellation|cancellation]]); `ReaderGone` and `Aborted` have
  none, and go straight to the kill. It goes once per process, to every
  `Address` `CollectState::addresses` enumerates: the pgid of an owner
  (`Group::owned`), whose signal is followed by `SIGCONT` so that a stopped
  member can act on it, else each live external by pid via `Watch::signal`, and
  only when the group `owes` the cause; and, beside either, each confined
  stage's envelope payload group, which bwrap's `--new-session` put in a
  session of its own. A key the tty already delivered (`pressed() >= cause`)
  leaves out the pipeline's own addresses, never the envelopes, whose session
  was never in the foreground group.
- The grace is at most `TEARDOWN_GRACE` (500 ms, a lone command's too), waited
  out a slice at a time (`drain_until`), filing each stage's terminal event and
  escalating as it goes: a filed panic to `Aborted`, a stronger cause to
  itself. The sweep before each wait leaves the sentinel unmarked, since a
  teardown cuts nothing at an edge: a shielded cleanup upstream of an edge
  whose reader has gone neither blocks on it nor is cut. Unless the teardown
  is `Aborted`, which no shield masks, a wait for live thread stages follows,
  bounded by the run's `cleanup_grace`, since a thread stage may be in a
  shielded cleanup whose children share the group.
- `kill_live` addresses the same set again: `SIGKILL` to the pgid, or
  `Watch::kill` by pid for a collector with no pgid of its own; each envelope
  by its own pgid; and the guest jail's cgroup of every live stage
  (`JailCgroup::kill`), for a member that `setsid()`'d out of the group. The
  unbounded `drain` after it has nothing left to block on.
- The anchor goes last, when `PipelineGroup` drops, after every stage handle:
  `AnchorProcess` ends and its own `Watch` is dropped, which reaps. The report
  reader's read end closes only on the anchor's own EOF, so it is never dropped
  while the anchor could still `SIGPIPE` on a stray write into it.

Because every collector's `cancel_all` reads that one table, a reader-gone
teardown kills outright at every nesting level: `!{ yes | cat } | head -1` ends the inner `yes` without a
catchable `SIGTERM` ever being attributed to it. What the collector sent is
read back in `fold`, so a pipeline external torn down for its own stage is
consumed, and the teardown's cause, `timed out` and status 124 for a wall, is
the pipeline's stop, exactly as a standalone external's
([[decisions/260905_one-delivery-path|one-delivery-path]],
[[internals/cancellation|cancellation]]).

**A dropped collector kills, joins no pump, and leaves its threads to their own
drops.** `CollectState::drop` covers the one end that drops rather than
observes: an unwind between launch and fold. When a stage is still unobserved,
a condition read off the collector's state and no flag a caller sets, it
writes `Terminated` on every live thread stage in one sweep and runs
`kill_live`, so the descendants below the collector die before any handle
waits. Each `JailCgroup`, live or filed, is finished as the collector drops it.
No pump is joined, and a pump a dropped stage holds is detached: a descendant
the kill missed could hold its pipe for ever. Each unobserved `ThreadStage`
cancels and joins itself in its own `Drop`, a slice at a time, sharing the
strike already made, so the joins share one grace.

A handle drops its hold on its outbound edge before it joins its stage
(`StageHandle` declares `held` before `kind`), so a thread stage blocked
writing to that edge fails on the broken pipe once its reader's own end
closes, instead of holding the join for its grace; a forced end has no
verdict to protect.
`PipeNode`'s field order carries the drop half: collector, then group. The collector's
kill must reach the pgid it named, and it is the anchor, alive or its zombie,
that keeps that pgid from being reused; the anchor is reaped last precisely so nothing that
could still need its pgid joinable is waiting on it first.
On Windows the grace is Ctrl-Break to every member of an owned group
(`break_pipeline_group`: one event per pid on the job's member list, nested
members included, each stage being its own console-group root), which the
anchor swallows; a pid address no event reaches is killed at once, as its
opening, so there is no grace to wait out, and the group's `Job` is released
when its owner drops (above).

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
