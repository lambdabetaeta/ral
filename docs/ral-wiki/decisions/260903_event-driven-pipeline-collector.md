---
status: active
---

# Event-driven pipeline collector

**Every event in a pipeline's lifecycle is an edge — a stop reported once, a
byte written once, a thread returning once — so the collector that used to
poll for them is now a pure fold over one channel.** `CollectState::step`
inspects the observation vector, the group's role, and the tracked stop
levels, and returns the `Effect`s a thin interpreter performs; `drive` is
`loop { for e in step(&mut st, rx.recv()?) { run(e) } }`. One dedicated
thread per blocking wait — a stage's own settlement, an external's stop, the
anchor's witness — replaces `CollectState::drive`'s 5–100 ms backoff,
`RunningChild::wait`'s own poll loop for a pipeline stage, and `StageGate`'s
20 ms condvar timeout for a thread's interior stop.

## Decision

- **The event sum is total, `Event::Settled(usize, StageObservation)` for
  both stage kinds — `Stopped`/`Continued`/`Witnessed` are `cfg(unix)`,
  nothing stopping on Windows.** A thread stage's own closure already
  computes its `StageObservation` in-process (phase 2). An external's own
  waiter thread cannot: an external's finish still needs `&Shell` for audit
  synthesis, exit-hint lookup, and sandbox-denial augmentation, and a
  producer thread holds none. The channel's actual wire item is therefore
  `Report`, one layer under `Event`: `Report::Settled` carries a `Settlement`
  — `Thread(StageObservation)`, already finished, or
  `External { name, failure }`, the raw `Settled<Option<CommandFailure>>` a
  waiter thread can compute without a shell. `CollectState::resolve` is the
  one place `&Shell` reaches it, turning a `Report` into the `Event` `step`
  folds over. This is the one seam where the implementation's wire type
  doesn't literally match the plan's `Event` — a Shell can't cross threads,
  not a redesign of what crosses the channel.
- **One waiter thread per direct external stage, the sole owner of its
  wait.** `command::RunningChild::run_pipeline_stage` blocks
  `ChildHandle::wait_tracking_stops` (`WUNTRACED | WCONTINUED`, retried
  across `EINTR` — the blocking peer of phase 1's non-blocking version, which
  it replaces) in a loop, reporting each `Stopped`/`Continued` edge through a
  callback and returning the terminal outcome. It tracks its own last-seen
  stop locally: a signal death immediately following a reported stop, with
  no `Continued` between, is reclassified as `StoppedThenKilled` — the
  collector's old `end_stopped`, a synchronous kill-and-reap racing nothing
  because it ran before the poll that would have seen the stop again, is
  gone; the identical verdict now falls out of the waiter's own local
  bookkeeping, since a stopped child SIGKILLed by anyone is
  observably "stopped, then killed" whoever sent the signal. Ending
  attribution reads a `kill_cause: CancelScope` fresh per stage — **not**
  `RunningChild`'s own `cancel` field, which is the *mooring's* scope,
  shared with every sibling stage and beyond it; an early draft attributed
  ending through that shared scope and one stage's reader-gone kill
  cancelled its unrelated siblings, surfacing as spurious "its reader ended"
  failures two stages away. `StageHandle`'s external case shrinks to
  `ExternalWaiter { join, kill_cause, pid }`: everything else the collector
  once read off the child directly (`try_settle`, a stop level) is the
  waiter's own business now.
- **The anchor's witness is two threads, not one, and not the plan's literal
  wording — and the anchor's own stop is a level from two edges, exactly
  like a member's.** The plan describes "one reader thread… (blocking
  read… plus a waitpid on the anchor for its stop)"; no single OS thread can
  block on a pipe read and a `waitpid` at once without a signal-multiplexing
  mechanism (a self-pipe, `ppoll` with an unblocked `SIGCHLD`) the plan
  never asks for and this phase does not add. Two dedicated threads — a
  report reader blocking to EOF on the anchor's stdout, an anchor waiter
  blocking `waitpid(WUNTRACED | WCONTINUED)` as the sole reaper of its pid —
  both report `Witnessed` and both terminate structurally: the report
  reader on EOF (the anchor's own exit closing its write end), the anchor
  waiter on any terminal outcome. The anchor waiter tracking `WCONTINUED`
  too, and reporting it as `Witnessed::Continued`, is not optional
  symmetry: the anchor is a member of the group like any other, and an
  early draft that tracked only the *members'* stop as a two-edge level
  while leaving the anchor's a one-shot signal could never clear the
  group's `parked` flag if the anchor's own stop was what last set it — a
  second, genuine Ctrl-Z would then never park. `AnchorProcess::finish`
  closes the release pipe, `CONT`s the anchor's pid alone, and joins both
  threads instead of reaping directly — reaping from `finish` and from the
  anchor waiter at once would be the very double-`waitpid` this whole plan
  exists to close out. The report reader's read end, living inside its own
  thread's stack, closes only once it observes EOF, so the old ordering
  hazard ("closing the parent's read end before the anchor is confirmed
  dead risks a `SIGPIPE` on a stray write") cannot arise by construction,
  not by a manual sequence.
- **A thread stage's interior stop gets a fourth kind of producer: a
  condvar, not a poll.** `StageStop` — a stage thread's own spawned child
  stopping, written by phase 4's own (untouched) `RunningChild::wait`
  poll loop — stays a cell for that writer, since touching that
  loop's shape is phase 4's job, not this one's. But the *reader* no longer
  polls it: `StageStop` gains a `Condvar`, `set` notifies it, and a
  per-thread-stage watcher thread blocks on `wait_for_change`, reporting
  each transition as `Event::Stopped`/`Continued` exactly as an external's
  waiter does. `StageStop::close` — the stage's own last act, before its
  `Settled` send — wakes the watcher a final time so it never outlives the
  stage blocked on an edge that will not come.
- **`step` is pure: filing an observation and updating this collector's own
  tracked levels are not `Effect`s; writing into *another party's* shared
  cell is.** The plan's `Effect` enum — `PauseGate`, `SigstopGroup`,
  `SigcontMember`, `CancelAll`, `Park`, `Done` — grows two members beyond
  the plan's literal list, `KillStoppedStage` and `ForwardStop` (both
  below), each earning its keep against this same line: a signal, a gate, a
  wake, or a write into state this collector does not itself own, crosses
  as an `Effect`; `self.observed`, `self.stopped`, `self.parked` — this
  collector's own bookkeeping — do not. **An early draft got exactly this
  line wrong twice**, both caught only by trying to write the
  transition-table tests the plan calls for: `on_stopped`'s
  joining-with-a-park arm called `park.stop.set(Some(sig))` — the *owning*
  collector's own `StagePark`, one level up — directly inside the fold,
  which a pure-`step` test cannot even observe (nothing about `step`'s
  return value shows it happened) and so cannot pin; forwarding the stop is
  now `Effect::ForwardStop(Option<Signal>)` (`Some` for the stop, `None` for
  its resume), and a test asserts `step` alone leaves the owner's park
  untouched. Separately, `kill_now` unconditionally raised
  the one forgiven ending for *any* `KillStage`-shaped kill, so the
  background-stop answer's own kill (below) silently forgave a verdict that
  should have surfaced as `StoppedByJobControl`.
- **`KillStage` is the reader-gone cascade's alone; `KillStoppedStage` is a
  second, distinct kill.** Both reduce to "kill this stage's pid, or
  cancel-and-wake this thread, right now," and an early draft reused
  `KillStage`/`kill_now` for both, reasoning that
  `CommandFailure::from_outcome`'s reader-gone forgiveness never applies to
  `WaitOutcome::StoppedThenKilled` regardless of which `Ending` is stamped
  — true, but beside the point: `kill_now` is also the *only* place
  permitted to raise `Ending::RalEnded(ReaderGone)`, the one forgiven
  ending, and a background group's stop-then-kill is not a reader-gone
  death — reusing the same method broke that invariant's literal truth even
  though the specific verdict it protects happened to survive by another
  route. `KillStoppedStage`/`StageHandle::kill_stopped` raises no ending at
  all, leaving the following `CancelAll(Terminate)`'s own `cancel_stages`
  to stamp one; a transition-table test asserts the two `Ending`s differ.
  The ownerless-stop rule (`SigcontMember`) is unaffected: reviving a
  detached member is not a kill, forgiven or otherwise.
- **A `Foreground` park needs one level, `parked`, guarding it — the load
  a polled `resume_all` used to carry for free.** `sleep 10 | cat`'s one
  Ctrl-Z is three edges: the anchor's own `Witnessed::Stopped`, and each
  external's own waiter's `Event::Stopped`. The polling collector could not
  have this bug — `resume_all` cleared every level synchronously on `fg`,
  before the next probe could read a stale one — but the event-driven one,
  with no `resume_all` left to call (each producer now reports its own
  `Continued` structurally), can: `drive` folds the *first* edge, returns
  `Drive::Parked`, and the other two sit queued in the channel; `fg`'s
  `SIGCONT -pgid` clears nothing on this side, and `drive`'s next `recv` is
  the stale second edge, parking the group right back — one `fg` per
  stopped member instead of one. `parked: bool`, set on the first edge a
  `Foreground` group's `on_stopped`/`on_witnessed` answers with `Park`, and
  cleared only once every tracked level — `stopped[ix]` for every member,
  `anchor_stopped` for the anchor — reads clear again, is what makes a
  second edge of the same stop answer with nothing while a genuinely later
  one still parks.
- **The cancellation timer is the one left, on every platform, and does
  only cancellation.** A low-frequency (200 ms) thread holds a clone of the
  mooring's own `CancelScope` and a `Weak` token to the collector; it exits
  once the collector is gone (the `Weak` fails to upgrade) or once it finds
  a cause to report, whichever comes first. It does not replicate
  `check`'s park-gate wait — that cooperative block belongs to whichever
  thread is actually evaluating a joining collector's own nested `drive`,
  and the event-driven model needs no proxy for it: the shared pgid's own
  `SIGSTOP` already freezes every process transitively, and a joining
  collector's `drive` loop simply goes idle, blocked in `recv`, until a real
  edge — its own member's `Continued` — arrives.
- **Cancellation reaches the pids this collector launched, not just the pgid
  it may own.** A joining collector's own group can neither `signal` nor
  `kill` — there is no pgid of its own to send either to — so a direct
  external stage nested under a `timeout` used to hang in `cancel_all`'s
  final drain until it exited on its own: `StageHandle::cancel` set only the
  waiter's `kill_cause`, an attribution nothing delivers. `cancel` now also
  signals the stage's own pid directly (the cause's signal, then `SIGCONT`,
  the same pair `PipelineGroup::signal` sends a whole pgid — factored out as
  `cause_signal` so the two stay one choice), and `cancel_all`'s kill step
  falls back to a pid-wise `kill_live_externals` wherever `group.kill()`
  would itself be a no-op; `CollectState::drop` takes the same fork for an
  unwinding joining collector. A nested pipeline's `timeout` now behaves
  exactly as a top-level one's does. Descendants of a killed external remain
  the group owner's to end, as for any stage's own kill — the same edge
  every POSIX shell has, and moot on Linux under confinement, where the
  stage's own jail cgroup kill already takes its descendants with it.

## Superseded and unamended

Supersedes the polling half of
[[decisions/260902_stages-are-threads|stages-are-threads]]:
`PipelineGroup::witness`, `StageHandle::probe`/`Probe`, `RunningChild::
try_settle`/`stopped_level`, `CollectState::pass`'s `Pass::{Advanced,Idle}`
and its backoff arithmetic, and the pipeline arm of `RunningChild::wait`'s
own poll loop are all deleted; the thread/process shape, the anchor's
existence, `GroupRole`, `StopPolicy`, and `ParkedPipeline` are unchanged.

Leaves phase 4 of `dev/docs/plans/260902_event-driven-pipeline-collector.md`
open, each small and independently addressable: standalone (non-pipeline)
`RunningChild::wait` still polls for its own cancel scope, since a blocking
wait there could not otherwise be preempted by a deadline or an upstream
cancel; the Windows stage-thread interrupt loop still retries
`CancelSynchronousIo` on its own 5 ms sleep; a killed thread's forgiven
`Break` still carries no mark of whether the kill or its own code produced
it, indistinguishable in `StageObservation` from an ordinary one.

## Risks accepted

- **The anchor's report-outranks-stop precedence is no longer enforced by a
  single poll's read order — it is now a genuine race between two
  threads.** A signal reported in the same instant as the anchor stopping
  can, in principle, be answered as a stop first. Accepted: the two are
  already rare and nearly disjoint in practice (a signal reaching a
  process about to be `SIGTSTP`'d by the same delivery), and getting this
  fully ordered again needs the anchor to fold both into one report
  itself — a change to the anchor's own protocol, out of scope here.
- **A leaked producer thread outliving its pipeline would be a defect, and
  the cancel timer is the one intentional exception.** Every other producer
  terminates on its own terminal event or a failed send; the timer alone
  waits out its own tick (at most 200 ms) after the collector is dropped
  before noticing and leaving. Bounded, not unbounded — but not instant
  either, unlike everything else this phase deletes a timer to achieve.
- **Thread count grows by one waiter per direct external stage, one watcher
  per thread stage, and two per owned group's anchor**, on top of the stage
  threads phase 1 already counts. Bounded by stage count, the same order the
  pumps already cost; no pipeline this shape supports has enough stages to
  make this material.
- **Kill by pid is no longer structurally immune to pid reuse.** The old
  `Child::kill` refused to signal a handle whose status was already
  recorded; `kill_stage_by_pid` signals a bare pid from a thread other than
  the one reaping it. The window is between the waiter's `waitpid` returning
  and its `Settled` being processed by the collector, while `stages[ix]` is
  still `Some`. Accepted as negligible on sequential-pid kernels; the exact
  fix on Linux is `pidfd_open` at spawn + `pidfd_send_signal`, macOS has no
  equivalent short of a WNOWAIT reaping protocol; to be taken up when phase
  4 touches the waiter.
- **The cancel timer watches the launch-time mooring's scope**, where the
  polling `drive` checked the scope of whichever mooring the caller passed
  (for `fg`, the REPL's current one). In practice the same root scope;
  noted as a semantic change.
- **A panic in the interior-stop watcher or an anchor witness thread loses
  edges, never a `Settled`.** Neither owns an index — only a stage's own
  closure or an external's waiter does, each behind its own `SettleOnDrop`
  — so the pipeline can go deaf to Ctrl-Z/Ctrl-C (a stop or a signal simply
  never arrives) but `drive` still reaches a stage's own end and cannot
  hang. Out of scope: a stack overflow or a double panic aborts the process
  regardless of any of this.
- **Edges from different producers are ordered only per producer.** After
  `fg`, one member's `Continued` can in principle overtake another member's
  `Stopped` from the same Ctrl-Z and re-park the group; the `parked` level's
  soundness assumes every `Stopped` edge of one stop arrives before any
  `Continued` of the resume, which holds unless a waiter thread is starved
  for the whole human-scale interval between Ctrl-Z and `fg`.

## Measured

Verified by hand alongside the suite: `!{ echo a; exit 3 } | head -1` exits
0, its writer's already-observed outcome untouched by the reader-gone
cascade that arrives one event later; `let n = !{ sh -c 'while :; do :; done'
| !{ return 5 } }` binds `5`, the killed producer's `SIGKILL` reclassified
and forgiven entirely inside `run_pipeline_stage`'s own local bookkeeping,
with no probe anywhere in the path; `yes | cat | head -1` exits 0, both
interior stages killed and forgiven independently over the same channel;
Ctrl-Z on an all-`ral` foreground pipeline (`!{ a } | !{ b }`) still parks
through the anchor's own waiter thread, `fg` still resumes it to completion.
`just ci` is green on this host; the Linux container was not run.

See also [[decisions/260902_stages-are-threads|stages-are-threads]],
[[internals/pipeline-execution|pipeline-execution]],
[[decisions/260820_a-stage-ral-stopped-has-no-failure|a-stage-ral-stopped-has-no-failure]]
(unchanged: the forgiven-death rule is stated once, above the transport that
realises a stage — only its trigger moved from a probe to an event).
`docs/SPEC.md` §7, §7.6, §11.6.
