---
verified_at_commit: dad6a1b3
verified_at_date: 2026-10-17
anchors: [Break::stopped, Escape::Stopped, Shell::poll, Shell::scope, Shell::shield, Mooring::home, CancelScope::shielded, ScopeId, POLL_SLICE, final_poll, Break::code, Break::of_child, CollectState::recv, CollectState::cancel_all, ESCALATION, forward_ambient, Ambient, ControlSender::forward_signals, ControlSender::hear, ControlSender::aim, interrupt_epoch, CancelScope, CancelCause, Terminate, DurableRoot, ForegroundScope, request_interrupt, request_root_cancel, INTERRUPTS, REQUESTED_ROOT, Mooring, run_under, Chrome, Scrollback::last_is_error, Shell::join_session, Shell::cancel_handle, interrupt_handler, sigint_handler, sigquit_handler, grace_signal, signals_of, gesture_signal, GESTURES, gesture, TerminalLoan, TerminalLoan::hear, TerminalLoan::reclaim, WaitOutcome::death, ChildEnd, WaitOutcome::classify, KILL_EXIT_CODE, CancelCause::code, Signal::status, RunningChild::wait, Membership::owes, break_pipeline_group, escalation_pending]
---

# Cancellation

**Stopping in-flight work is one delivery mechanism — a *cooperative,
cause-bearing scope tree* that asks the evaluator to unwind at its next poll
point — backed by an *escalation ladder* that forces an exit when a user
insists.** A signal handler holds neither a `Shell` nor a scope, so it raises
one of two process-lifetime **ambient causes**, and no scope folds either: the
engine's host forwards each to the engine as `Control`, which strikes the
scope tree like any other cancel
([[decisions/260706_signals-are-causes|signals-are-causes]]). The host is the
identity front-end — REPL, batch, exarch — or, for a wire engine, the engine
process itself. Both live in `core/src/process/signal.rs` (see
[[map/core/io-process|io-process]]); the gestures that drive them differ per
host.

The two pieces answer different questions:

- **The scope tree** (`CancelScope`) answers *"which subtree should unwind, and
  why?"* — a structured-concurrency primitive that names a *cause* and reaches
  exactly the workers that inherited the cancelled scope. It is the only thing
  `Shell::poll` reads, and what it raises is a *stop*: an escape naming its
  owner, which no `try` catches
  ([[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]]).
- **The ladder** (`ESCALATION: AtomicU8`) answers *"is the user escalating
  toward kill?"* — the third delivery forces `_exit(128 + sig)`. It is a blunt,
  host-agnostic floor for a process whose cooperative delivery is wedged, never
  a delivery mechanism itself.

## The escalation ladder

The platform handler `fetch_add`s the ladder on every delivered termination
signal; the third hit calls `libc::_exit(128 + sig)` — bypassing `atexit` so a
wedged process always dies. Nothing else reads it for control flow: `clear()`
resets it at acknowledgment boundaries (a fresh prompt, a run compile, a
session reboot), and `escalation_pending()` exposes it for observability only.
Being process-global and cleared by every run compile, it is nothing a test
can assert on beside a test that runs source.

Windows' console handler has the same shape: the first two console events
raise the interrupt, the third `ExitProcess`es with its status. Neither
ladder signals a process: a child hears ral's teardown only from the scope
that owns it, so nothing fans out around the tree.

**The force-exit floor is reachable only in non-interactive paths.** The `ral`
batch launcher binds SIGINT to `handler` (`batch.rs`, `install_handlers`); the
interactive REPL rebinds SIGINT to the non-escalating `interrupt_handler`
(below; on Windows, `install_interrupt_handler`), which never touches the ladder. So repeated Ctrl-C at an interactive prompt is cooperative, never a
hard kill — the escalation belongs to batch scripts and to external
SIGTERM/SIGHUP.

This is the
[[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
discipline taken to its end state: the user-facing interrupt writes a cause,
and *only* a real delivered signal walks the ladder.

## The scope tree and its cause lattice

A `CancelScope` is a node `{ flag, cancelled_at, parent, shield }` in a tree of
`Arc`-linked nodes. **Its cancellation is a *join*:**

    j(s) = ⨆ over chain(s) of flag(n)

- **`cancel(cause)`** sets `cancelled_at` (once) and then `fetch_max`es the
  flag — cancellation is one-way and *monotone*: a later, weaker cause can
  never mask a stronger one already in force. The instant is written first, by
  the same writer in the same act, so a reader that sees the flag sees it.
- **One private `resolve`** folds the chain root-first; `in_force`, `cause`,
  `is_cancelled` and `poll` are its readers, and every part it reads is
  private to `cancel.rs`. It mutates nothing. No mutex, no allocation beyond
  the chain walk.
- **The name.** `in_force` returns the join *and* `ν`, the **outermost** node
  whose own flag attains it — a declared tie-break, not a record of who struck
  first. A `ScopeId` (pointer identity) is that name; a stop carries it for its
  owner. A cause the collector writes on a stage that an ancestor already
  carries therefore renames nothing.
- **Six minting points**, each a frame or a base: the session root
  (`DurableRoot::new`); a worker's node and the session anchor
  (`DurableRoot::worker`); a host's dispatch scope (`run_cancel_handle`, the
  anchor's child); a run's node (`dispatch`, the base of a run); a stage's node
  (`launch_thread_stage`, a child of the *current* scope, so a pipeline inside
  a cleanup is masked with it); a cleanup's shield (`Frame::Cleanup`,
  `shielded(grace)`).
- **One writer per node.** The root by SIGTERM/SIGHUP/Ctrl-`\`; the dispatch
  scope by the host's interrupt; a run's node by its wall and by the key its
  terminal tenant heard; a worker's by `cancel`, a `race` loss, its lease, the
  session wipe; a stage's by its **collector alone**, with the cause of
  whatever teardown it runs (`ReaderGone` for a reader that left, `Aborted`
  for a panic, `Cancelled` for a sibling's `exit`, `Terminated` for the
  anchor's death or the collector's own drop, the cause it is tearing down
  for otherwise); a shield by nobody.
- The cause is an escalation order, `CancelCause`:

  | cause | value | meaning | who raises it |
  |---|---|---|---|
  | `ReaderGone` | 1 | a pipeline stage's reader has ended | the pipeline collector |
  | `Interrupted` | 2 | user asked the foreground to stop | Ctrl-C / Esc / batch SIGINT |
  | `Cancelled` | 3 | a targeted worker teardown | `cancel <handle>`, `race` loser |
  | `TimedOut` | 4 | a wall-clock / lifetime ceiling expired | `process::deadline` |
  | `Terminated` | 5 | the process was asked to shut down | SIGTERM / SIGHUP |
  | `Aborted` | 6 | the session root is being reaped | Ctrl-`\` |

`poll` mints `Break::stopped(cause, owner)`, an escape; its message is
`CancelCause::message`, the one vocabulary every poll point shares.

### Two typed scopes name the one invariant

The tree's load-bearing rule — *a run's foreground scope is always a descendant
of the session's durable root* — is spelled in the type system, not left to
discipline ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).

- **`DurableRoot`** (`shell.session.root`) is minted once per `Shell`. Detached
  workers — `spawn`, `watch` — parent under it, so a *foreground* cancel
  never reaches them ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).
- **`ForegroundScope`** (`Mooring::home`) is the run's work scope. It can be
  minted *only* from a `DurableRoot` (or by nesting another foreground), so an
  unrelated root can never be installed as a foreground by accident.
- `DurableRoot::worker` mints the scopes beside a run: shell init's anchor and
  `spawn_thread`'s worker. A run's frame nests under the anchor, so
  cancelling it never reaches a worker, while cancelling the root reaches both.
- **The tree is the runs' dynamic extent.** `Shell::dispatch` nests each entry
  under the frame the run door displaces, not beside it under the root, so a
  nested run observes what encloses it — its outer run's interrupt, and its
  outer run's wall elapsing
  ([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]).
  **A pipeline mints one node per thread stage**, under the current scope,
  which its collector alone writes; `CollectState::drop` writes `Terminated` on
  every live thread stage in one sweep, then kills whatever it still holds
  unobserved: the owned pgid, or each live external by pid (see
  [[internals/pipeline-execution|pipeline-execution]]).

## The ambient causes

A signal handler must not lock and cannot hold a `CancelScope` by value, so it
raises a cause on a process-lifetime `static`, and a host thread hears it off
signal context. The two are different kinds of proposition.

- **Shutdown is absolute.** `request_root_cancel(cause)` is one `fetch_max` on
  `REQUESTED_ROOT`. Once raised it holds for every listener forever, so a
  SIGTERM delivered before a host listens still reaches it.
- **An interrupt is temporal.** `request_interrupt()` ticks `INTERRUPTS`; a
  listener hears only the interrupts raised after it registered, having been
  aimed at the run in flight then.
- **No scope folds either.** `process::forward_ambient` hands a host thread
  each cause as it lands (`Ambient::Interrupt`, `Ambient::Root(cause)`), the
  handlers staying async-signal-safe and the reaper's wake running the scan.
  `ControlSender::hear`, also used by `forward_signals`, maps a root
  `Aborted` to `Abort` and any other root cause to `Terminate`, which cancel
  the durable root, and an interrupt to `Control::Interrupt(id)`, naming the
  dispatch its front-end has in flight. The REPL and batch hold one for the
  session, and exarch forwards to its trunk's `Agent` (`signals::face`); a wire
  engine hears only its own process's shutdown, since an interrupt is aimed by
  the front-end that sent the dispatch. So a process hosting many engines —
  exarch, a test binary — never has one signal reach them all.
- **A dispatch is born at an instant.** Forwarding lags the key by two thread
  hops, so the dispatch in flight when an interrupt *arrives* need not be the
  one in flight when it was *struck*: a Ctrl-C at an idle minimal prompt, where
  the canonical tty raises a real SIGINT, would land on the next line.
  `dispatch_to_report` aims its front-end's `ControlSender` at each dispatch it
  sends (`aim`), sampling `interrupt_epoch` under the lock `hear` reads it by,
  and `hear` sends `Interrupt(id)` only if the epoch has moved since. This is
  the birth instant of
  [[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]], which the
  move to `Control` had dropped, kept now per dispatch at the front-end rather
  than per frame. A key-driven `interrupt()` names whatever is in flight now.
  The engine strikes only the id named and foretells one not yet arrived, as
  it does a `Cancel`, so a late `Interrupt` finds a stale id and strikes
  nothing.
- **An idle Unix REPL is woken after its root is cancelled.** Its ambient
  forwarder calls `hear` before `Shutdown::end`: both readers are interrupted
  with thread-directed SIGINT until their exclusive registration ends. The
  minimal reader leaves stdin's shared flags untouched and checks shutdown
  after `EINTR`; the interactive handler omits `SA_RESTART`. Repeated delivery
  covers cancellation just before a blocking syscall or before rustyline
  installs its handler. A new read refuses a standing shutdown. The loop checks the root
  again after input returns, preserving its cause's status and the ordinary
  history, worker, and terminal cleanup
  ([[map/repl/frontend|frontend]]).
- **Sharing, not shadowing.** A nested run nests under the frame the
  interrupt strikes, so a Ctrl-C mid-nest unwinds the whole nest, as a POSIX
  shell's does.
- **A worker hears only the root**: `Terminated` reaches it, an
  interrupt, striking a run's frame, cannot.
- **A forked session** (`Shell::bequeath` and `Shell::child_of` — exarch's sub-agents) hears no
  signal at all, and its host cancels it through a clonable handle on its
  durable root (`Shell::cancel_handle`)
  ([[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]).
- **An aside is inside the session.** A second `Shell` the engine runs beside
  its session — a hook whose registered policy says `aside`, as the REPL's
  buffer-change hooks do while readline runs — *shares* the session's
  `DurableRoot` (`Shell::join_session`), so a `cancel_handle` cancel reaches
  it. It mints its frames under its own anchor, so it can neither absorb an
  interrupt that struck the run it was called from nor keep it from that run.

This is the seam the
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] ADR calls
"cancel translation": the semantic collapse is onto the scope tree, never
into the signal handler.

## Where cancellation is observed

A cancel is a *request*: it flips a flag, and nothing stops until a reader
samples its chain. There are two kinds of reading, and nothing is ever
delivered, so nothing can be lost.

**Step boundaries.** `Shell::poll` reads the current scope and raises the stop
in force, at:

- the **machine's step arms** (`evaluator/machine.rs`) — the β-step, `Bind`,
  `App`, `Rec`, `Source`, and the exec step each poll, so any loop of `ral`
  calls is preemptible (the original
  [[decisions/260504_hot-path-cancellation|hot-path-cancellation]] insight);
  a `?` chain's advance from one arm to the next is nested `try` applying a
  handler thunk, so it polls at the same β-step;
- the **iterating builtins** (`builtins/collections.rs`, `builtins/concurrency.rs`)
  — `map`/`filter`/`each`/`range` and the worker-join sweep poll between
  elements;
- **pipeline launch** (`runtime/pipeline.rs`, `runtime/pipeline/launch.rs`) —
  before and between stage spawns;
- a base's **closing poll** (`final_poll`), the first half of its return
  clause: the top of a run and a worker read their node once more after the
  body settled, so a stop that landed after the last poll is not lost. The
  run's wall is disarmed first, and a firing already claimed is waited for on
  its own `fired` flag, so nothing fires after this poll.

**Sliced waits.** Every blocked operation of ral's own is a sequence of (read
the scope; wait at most `POLL_SLICE`, 10 ms), the read first so an
always-ready stream still polls once per chunk: a pipe write
(`Sink::write_under`) and read (`read_interruptible`), a child's wait
(`RunningChild::wait`) and its pumps' drain (`WaitedChild::settle`,
`Pumps::join_under`), the collector's `recv`, the enquiry park, and the
`await` sweep. The slice is the sampling period, not an end-to-end bound: the
latency to a stop is the slice plus the step in progress plus the scheduler.
There is no watch table and no wake. Windows pipes cannot be waited on with a
timeout, so there a blocked thread stage or pump is interrupted
(`CancelSynchronousIo`) instead, and the aborted call re-reads its scope; a
write retried under a shield may repeat a prefix, the one open gap
([[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]]).

A computation that never reaches either (a tight Rust loop inside one
builtin, a host wait outside the sliced rule: a stopped tty, an inherited
terminal stdin, `ask`, a file read) is not interruptible by the scope path —
the contract is *cooperative*.

### The two cells

The **owner's node** is the base of a machine's tree — a run's, a worker's, a
stage's — and is run-invariant, so it lives on `Mooring::home`. It is what
*writes* aim at: the key a terminal tenant heard, a stage's group membership,
a lease and a `cancel`. The **current scope** is a descendant swapped by
frames; it lives on `Shell::scope` and is what *reads* consult: `poll`, every
sliced wait, a child's attribution, the collector's loop. At a base the two
coincide; inside a cleanup the scope is a shield under the node. A write never
aims at the current scope, so nothing ever writes a shield.

A stop at a base is consumed iff it names that owner's node: a worker turns a
stop naming its own node into the handle's `WorkerStopped` error; a collector
forgives every stop naming one of its stages' nodes; every other node is the
host's, and a stop naming one reaches the top uninterpreted, read as
`Ending::Stopped`.

### The shield

A `guard` cleanup runs under `Shell::shield(mooring.cleanup_grace)`: the
current scope becomes `scope.shielded(grace)` for the cleanup's extent and is
restored on pop and abandon, as `Frame::Capture` restores `io.stdout`.
Everything launched inside — a pipeline's stages, an external child, a nested
machine — reads the current scope and is masked with it. A shielded node's
fold admits from its ancestors only `Aborted` and, once `since + grace` has
passed, everything; `since` is the earliest `cancelled_at` on the chain above
it, and the grace is capped by the nearest enclosing shield's. Its own flag is
written by nobody, so a descendant's own cause stays visible. Every cleanup of
one unwinding therefore ends by one instant, and a run ends within `wall +
grace + TEARDOWN_GRACE`. The shield is cooperative: a signal the tty or the
collector sends to the group still reaches a cleanup's children, and a child
that dies of it is a process failure, since the current scope carries no cause
to attribute it to
([[decisions/261016_a-cleanup-is-shielded-within-its-grace|a-cleanup-is-shielded-within-its-grace]]).

The collector honours it in its teardown: `cancel_all` writes its cause on
every thread stage, waits for live thread stages, bounded by the grace, before
the group kill; a panic or a stronger cause mid-teardown escalates over the
survivors; an abort grants no grace and cuts the waits short; the teardown's
cause is a candidate verdict at the fold, the collector's own closing
observation.

### External children: teardown by cause

The process-wide reaper ([[map/core/io-process|io-process]]) posts a child's
exit onto `RunningChild::wait`'s own channel, and the wait receives a slice at
a time, reading the current scope between slices: a cause in force starts the
teardown, and the grace wait inside it is sliced too, ending at once under
`Aborted`. The teardown is *cause-directed*,
and `grace_signal(cause)` (`process/signal/unix.rs`, and its Windows twin)
is the one table that directs it — read by `RunningChild::terminate` and by
the pipeline collector's `cancel_all` alike:

- **`Interrupted`** → SIGINT-first, a bounded `TEARDOWN_GRACE` (500 ms), then a
  group (or, ungrouped, pid-`Watch`) SIGKILL — a child that traps SIGINT
  still dies, and its grandchildren with it.
- **`Cancelled` / `TimedOut` / `Terminated`** → SIGTERM-first with the same
  grace then the same kill — decisive, without pretending to be a user
  keystroke; a `Terminated` hands the tree the very signal the supervisor sent
  ral.
- **`ReaderGone` / `Aborted`** → `None`, an immediate kill with no grace. A
  reader-gone cut must not hand the verdict back to the producer's own
  disposition, and an abort has no grace to offer.
- **On Windows** every graceful cause opens with Ctrl-Break, named by the
  `STATUS_CONTROL_C_EXIT` it leaves, and only an owned console group takes it
  (`break_pipeline_group`: one `CTRL_BREAK_EVENT` per pid on the job's member
  list — every stage is spawned `CREATE_NEW_PROCESS_GROUP`, so each is its
  own console-group root, and a nested member is on the outermost owner's
  list); a pid gets the kill alone — the Windows `Watch` has no signal, and
  an `Inherit` child sits in ral's own console group, where no event reaches
  it without reaching ral. The pipeline anchor swallows every console event,
  so a group's grace never ends the process holding it open.

A process group is its owner's to signal: a child leading its own
(`Group::Owns`) is signalled and killed whole, while one that joined a
pipeline's from inside a stage thread (`Group::Joins`) opens with a grace
signal only for a cause `Membership::owes` — one stronger than the stage's
scope holds — every cause the stage holds being one the pipeline's owner
delivers to the whole group once.

Every external wait goes through this one `recv`/`terminate` shape — the
interactive REPL foreground included. ral does not suspend
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]): a stop
never reaches this channel at all, since the reaper answers it with
`SIGCONT` on its own before any subscriber sees one — a stop is never itself
a cause for teardown. A foreground external still gets its Ctrl-C from the
kernel directly — it holds the lent terminal — and the key is heard back by
the loan when the terminal returns (§Attribution); a SIGTERM delivered to
*ral* preempts even that wait through the root cause, read at the next
slice.

## One status per fact

**A run's ending is a function of its cause, never of where the cancel
landed.** A cancel that lands before a spawn meets a poll point; one that
lands after meets a torn-down child. Both doors mint the one constructor
`Break::stopped(cause, owner)`, so the two cannot disagree. `Break::code` is
the one table: an error's code is its `Status`'s, an `exit n` is `n`, a stop
is its cause's. `Status` is a two-level sum, each level with its own messages
and hints:

    Status         = Raised(i32) | WorkerStopped(WorkerStop) | Process(CommandFailure)
    CommandFailure = ExitCode(i32) | Signal(Signal) | Spawn(SpawnFailure)

`WorkerStopped` is the one stop that becomes an error: a worker's own boundary
converts a stop naming its node, and `await`/`poll` report it as the awaiter's
failure. Each fact owns its code (`CancelCause::code`, `SpawnFailure::code`,
`CommandFailure::code`, `Signal::status`/`Signal::of_status` for `128 + n`);
a signal handler's `_exit` reads `CancelCause::code` directly:

| break | code |
|---|---:|
| `Raised(n)`, `ExitCode(n)`, `exit n` | n |
| `Signal(s)` | 128 + s |
| spawn: not found | 127 |
| spawn: any other | 126 |
| `stop Interrupted` | 130 (`128 + SIGINT`) |
| `stop Cancelled`, `WorkerStopped(Cancelled)` | 143 (`128 + SIGTERM`, what `kill` sends) |
| `stop TimedOut`, `WorkerStopped(TimedOut)` | 124 (as `timeout(1)`) |
| `stop Terminated` | 143 (`128 + SIGTERM`) |
| `stop Aborted` | 131 (`128 + SIGQUIT`) |
| `stop ReaderGone` | 141 (`128 + SIGPIPE`) |

Errors reach ral code as the error record's `reason`
(`types/error/record.rs`, `Reason`), of which `status` is the projection;
`` `cancelled `` and `` `timed-out `` are the two `WorkerStopped` facts. A stop
has no record: no `try` sees it.

### Attribution

A child's death passes through `WaitOutcome::classify(cause, enveloped)`,
which yields a transient `ChildEnd`: `Failed(CommandFailure)` or
`Cancelled(CancelCause)`, and `Break::of_child` maps the second to
`Break::stopped` carrying the join of the current scope, exactly as a poll
point does. `cause` is the strongest in force on the *current scope* when the
death is observed, not only what ral sent: under a shield a key the tenant
heard is no cause, and the death is a failure.

**A death is a cancellation iff it is by a signal of a cause in force.**
`WaitOutcome::death(enveloped)` is the one reader of a death — a signal
death's signal, or an enveloped `Exited(128 + n)`'s `n` — and the death is
`cause`'s iff that signal lies in `signals_of(cause)`: the cause's
`grace_signal`, its key `gesture_signal`, and the kill that ends every
teardown — one function over the platform's three tables. With no cause in
force, no death mints one: every signal death is the child's own `Signal`, a
key's included, and an exit is never a signal — `exit 130` is a choice. A
SIGSEGV under any cause stays `Signal`.

| cause | `signals_of` (Unix) | `signals_of` (Windows) |
|---|---|---|
| `ReaderGone` | SIGKILL | `KILL_EXIT_CODE` |
| `Aborted` | SIGKILL, SIGQUIT | `KILL_EXIT_CODE` |
| `Interrupted` | SIGINT, SIGKILL | `KILL_EXIT_CODE`, `STATUS_CONTROL_C_EXIT` |
| `Cancelled`, `TimedOut` | SIGTERM, SIGKILL | `KILL_EXIT_CODE`, `STATUS_CONTROL_C_EXIT` |
| `Terminated` | SIGTERM, SIGHUP, SIGKILL | `KILL_EXIT_CODE`, `STATUS_CONTROL_C_EXIT` |

**Only a terminal ral lent can report a key pressed on it.** A key is what a
terminal sends: the `GESTURES` table in `process/signal/unix.rs` — SIGINT →
`Interrupted`, SIGQUIT → `Aborted`, SIGHUP → `Terminated`; no terminal sends
SIGTERM. Its reader `gesture` is read only by the `TerminalLoan` (`process/foreground/unix.rs`), the one
lending of the session's `TerminalLease` to a group, for a run. While it is
lent the key reaches the tenant's group and never ral, so the loan hears it
back from the tenant — `TerminalLoan::hear` — and **strikes it on the node it
was lent for (`Mooring::home`) as it hears**:

- a **lone command** is heard at `reclaim`, the instant it is dead, from its
  own `death`; `command.rs` then reads the *current scope* once, so Ctrl-C
  ends the command stopped with `Interrupted` and the run is stopped — unless
  a shield masks it, when the death is the command's own `` `signaled 2 ``
  and the stop follows at the cleanup's first poll after the grace;
- a **pipeline**'s loan is held by its collector: the anchor reports each
  signal it swallowed raw as `Event::Heard(signal)`, which strikes the node
  and records `pressed`; the teardown follows from the scope read at the next
  slice, which is what lets a shield hold it off. `pressed` is read only to
  skip a duplicate grace signal, never to attribute a death. The loan's drop
  restores the terminal and strikes nothing.

The node the loan strikes is always the run's own, never the current scope:
neither a stage's child nor a stage's pipeline is lent, so the strike reaches
every stage through the chain and the run's closing poll alike. What the loan
cannot settle is an external inside a *stage body*: a `Joins` member whose
waiter reads its scope as the reaper posts its death, racing the strike.
Lose the race and the stage reads `` `signaled 2 `` — a failure ranking under
the stop the run's own poll then raises, so the run stops either way.

A key ral's own frontend re-creates onto a lent group
(`interrupt_foreground_child`) is heard like the kernel's: it is the user's.
Without a loan — exarch, a capture, a stage thread, a worker's pipeline — a
heard signal is inert, so `try` stays total over everything else a child can
do, a child that kills itself with SIGINT included.

Two shapes were rejected
([[decisions/260924_the-lent-terminal-returns-the-gesture|the-lent-terminal-returns-the-gesture]]):
ral keeping its own pgid for lone children, which would make ral the ear of a
program that reads Ctrl-C itself; and a seat enum recording where a child
sits, a fact nobody holds.

The escalation ladder's forced exit status is its handler's own cause's —
130 for SIGINT, 143 for SIGTERM and SIGHUP. Windows has no signals to read,
so ral ends every process it tears down — `TerminateProcess` and
`TerminateJobObject` alike — with one `KILL_EXIT_CODE`; `death` there reads
that and the console's `STATUS_CONTROL_C_EXIT` and nothing else. The latter
is what Ctrl-C and Ctrl-Break both leave, so it is every graceful cause's —
a `TimedOut` teardown reads `timed out`, never an exit status — and with no
such cause in force it is displayed as "ended by Ctrl-C". A console is never
lent: the Windows `TerminalLoan` is uninhabited. A cancelled child carries no
hint.

## The gestures, per host

The same two mechanisms are driven by different keys on different surfaces.

| gesture | surface | what fires | effect |
|---|---|---|---|
| **Ctrl-C** | ral REPL, mid-eval | SIGINT → `interrupt_handler` | `request_interrupt()`, forwarded as `Control::Interrupt` — a pipeline's externals hear it through the collector; **counter untouched** |
| **Ctrl-C** | ral REPL, idle prompt | line editor reads it as a byte | abandons the partial buffer; nothing is in flight, so nothing is sent; no signal |
| **Ctrl-C** | ral REPL, idle minimal prompt | canonical tty → SIGINT → `interrupt_handler` | the tty drops the partial line; `request_interrupt()` is forwarded, but every later dispatch is born after it, so it strikes none |
| **Ctrl-`\`** | ral REPL, terminal not lent | SIGQUIT → `sigquit_handler` | `request_root_cancel(Aborted)`, forwarded as `Control::Abort` — reaps foreground *and* every worker, latching if idle; the REPL loop observes the sticky root and exits |
| **Ctrl-C / Ctrl-`\` / hangup** | any ral host, terminal lent to a command or pipeline | tty → the tenant's group (a hangup also reaches ral, as session leader) | the `TerminalLoan` hears it from the tenant — its death, or the anchor's `Heard` — and strikes `Interrupted` / `Aborted` / `Terminated` on the run's node as it hears: the job is stopped, `try` never sees it, the session lives |
| **Ctrl-C** | ral batch / `-c` | SIGINT → `handler` | `request_interrupt()`, forwarded as `Control::Interrupt`, + ladder `+1`; third press `_exit`s |
| **SIGTERM / SIGHUP** | any ral host | `handler` (term disposition) | `request_root_cancel(Terminate)`, forwarded as `Control::Terminate` — foreground and workers unwind, externals torn down SIGTERM-first, exit 143; ladder `+1`, third delivery `_exit`s |
| **Ctrl-C / Ctrl-Break** | ral on a Windows console | console event → the `ctrlc` handler | `request_interrupt()`, forwarded as `Control::Interrupt`, + ladder `+1`; third event `ExitProcess`es with 130. No console event is sent to any group: the struck run's teardown breaks the groups it owns, and a worker's are never reached |
| **Ctrl-C / Esc** | exarch TUI, active exchange | `Agent::interrupt` on the focused agent (reached through that tab's own `Weak`); the trunk also `cancel::raise_interrupt` | cancels the focused agent's `Token` and, as `Control::Interrupt`, its dispatch in flight; on the trunk, additionally `interrupt_foreground_child` (Unix) or `request_interrupt()` (Windows) |
| **Ctrl-C / Ctrl-Break** | exarch on a Windows console | console event → `console_ctrl_handler`, ahead of ral's | `request_interrupt()`, reported handled so ral's ladder never sees it; `face` forwards it to the trunk's `Agent::interrupt` |
| **Ctrl-C / Ctrl-D** | exarch TUI, idle prompt | key table → quit | drops the TUI guard; no cancellation |
| **Ctrl-C / Ctrl-D / Esc** | exarch TUI overlay | key table → close overlay | returns to the underlying prompt / exchange; no root cancel |
| **async SIGINT** | exarch | ral's non-escalating `interrupt_handler` | forwarded by `signals::face` to the trunk's `Agent::interrupt` |
| **async SIGTERM / SIGHUP** | exarch | ral's `handler` | forwarded to the trunk's `Agent::cancel(Terminate)` — its `Token` and `Control::Terminate`; ladder `+1` |

### ral interactive signal dispositions

`boot::setup_signals` (`ral/src/repl/session/boot.rs`)
fixes the interactive dispositions, each through core's `process::ignore` /
`process::install` — the only door a disposition changes by, and the one that
snapshots the inherited `SIG_IGN`s (once, before the first change) so every
child gets them back in `reset_child_signals`:

- **SIGINT → `interrupt_handler`** (the `sigint_handler` it names). It raises
  the interrupt and does nothing else — no delivery of its own. The session
  forwards it as `Control::Interrupt`, and ral's own delivery to a pipeline's
  processes is the cancel tree alone: the collector's `recv` reads the struck scope between
  slices, mints `Event::Cancelled`, and tears the group down with exactly one grace signal per
  process ([[internals/pipeline-execution|pipeline-execution]],
  [[decisions/260905_one-delivery-path|one-delivery-path]]). A foreground
  pipeline's externals usually hear the kernel's own copy first, delivered by
  the tty to the pgid the terminal is lent to; the anchor reports it and the
  pipeline's loan hears it as the key, so teardown does not re-send it.
  Raised while idle, it strikes only the settled run's dead scope, so the
  next run never sees it — and a worker, off every run's chain, is
  spared outright.
- **SIGQUIT → `sigquit_handler`**, the louder "reap everything" gesture
  ([[decisions/260629_agent-binding-reaping|agent-binding-reaping]] keeps it as
  *cancellation*, never deletion). It is a cooperative `request_root_cancel`, not
  the default core-dump — so it satisfies "Ctrl-`\` must not core-dump the shell"
  *by reaping*, not by ignoring.
- **SIGTERM/SIGHUP → `handler`** — translates to a root `Terminated` (the whole
  session unwinds, the REPL loop exits 143) and walks the escalation ladder;
  **SIGTSTP/SIGTTOU/SIGTTIN/SIGPIPE → `SIG_IGN`** — the shell answers a stop
  reaching one of its own children through `waitpid`/`SIGCONT` rather than
  ever being stopped itself, and rewrites terminal state without being
  stopped ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).

### exarch: the forwarded signals and the per-agent token

exarch layers a *per-agent* cancellation `Token` over ral's machinery
([[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]).

- **`Token`** is an `Arc<AtomicU8>` carrying a `CancelCause` (`0` while
  uncancelled), one sticky token per agent for its whole life; the attend loop
  threads clones through `deliberate`/`run_batch`/tools, so cancelling any share
  halts that agent's exchange (provider streaming, invoked tools). A genuine
  exchange boundary `Token::reset`s the flag, so a prior exchange's Esc never
  bleeds into the next.
- **The tree cascade is two-layer.** `Agent::cancel_tree` (behind
  `` exarch-agents `cancel ``, the per-agent idle lease, and the
  `/clear`/`reply` reaps) cancels each descendant's `Token` *and* its own
  engine's `DurableRoot`, through `reach: InterruptTarget` on the `Agent` — the
  seat's current `ControlSender`, whose `terminate` means the same under either
  carrier, republished at every `/clear` so it never goes stale. The
  token stops the attend loop between steps; the root cancel unwinds a `ral` eval
  already in flight at the evaluator's poll points — without it, a cancelled
  agent would grind to its tool's `timeout_secs` wall before noticing. No root
  carries a lease, so a `terminate` lands on the trunk only as it ends. What
  `raise_interrupt` still uniquely covers on the trunk is the SIGINT re-created
  for a foreground external child
  ([[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]).
- **A cancel may precede the run it names.** Both eval-layer channels into an
  identity transport — a `Control::Cancel` naming a dispatch id, and an
  observing host's `InterruptTarget::interrupt` — land on the scope
  `IdentityTransport::dispatch` mints *ahead of the engine lock*, never on the
  run's own frame. That ordering is the whole point: a dispatch parked on the
  lock has no frame yet, and the cell that named the last run would answer for
  it. Cancellation being sticky and folded live along the chain, the frame is
  born a descendant of an already-cancelled scope and unwinds at its first poll
  point. A cell published only once the frame exists
  loses exactly the cancels raised in that window; the run then survives its
  interrupt, and only the cooperative `Token` — read between steps — ends the
  exchange, a tool's whole `timeout_secs` later.
- **`install`** points SIGINT at ral's non-escalating `interrupt_handler`,
  leaving SIGTERM/SIGHUP on ral's `handler`, so the escalation ladder survives.
  Install order matters — ral's handlers first, then exarch's — and
  `boot::face_process_signals` (`exarch/src/boot.rs`) runs it once, at the process entry; each
  site that launches a process trunk holds `signals::face` for the attend, which
  forwards the ambient causes to that trunk's `Agent`. `/clear` touches no
  process-wide state: SIGINT no longer ticks the ladder, so there is nothing
  for a rebuilt context to reset.
- Raw mode disables `ISIG`, so a TUI keystroke is *not* a kernel signal. The TUI's
  key table (`exarch/src/tui/tui_loop.rs`) separates UI shape from cancellation:
  idle Ctrl-C/Ctrl-D quit, overlays close, and only active-exchange Ctrl-C/Esc
  route to the focused agent's own `Agent::interrupt` — every tab, the trunk included.
  The trunk's tab additionally raises `raise_interrupt`, since nothing else
  delivers the foreground external child's SIGINT: it re-creates the SIGINT the
  kernel would have sent a foreground *external* child via
  `interrupt_foreground_child`. On Windows it raises ral's interrupt alone:
  a tool child hears Ctrl-Break only from its run's teardown.
- A cancelled turn is a distinct TUI `Chrome::Cancelled`: the rail maps it
  to the error `╳` so the broken-off work is visible, while
  `Scrollback::last_is_error` matches only the fold's `Error`,
  `ProviderError` and `Stalled` blocks, keeping the matrix's failure cell for
  actual failures.

## Why interactive Ctrl-C cannot force-exit

A deliberate asymmetry worth stating plainly: **the third-signal `_exit` floor is
unreachable from an interactive prompt.** Interactive SIGINT goes to
`interrupt_handler`, which never ticks the ladder; the TUI's active-exchange
Ctrl-C goes to
`Agent::interrupt` (and, on the trunk, also `raise_interrupt`), neither
of which ever touches the ladder. Repeated presses re-write the same cause
(`fetch_max`), never escalate. The hard
floor exists for batch scripts (`handler`) and for external SIGTERM/SIGHUP. Interactive cancellation is cooperative
by construction; the root-reap gesture is REPL Ctrl-`\`, not a TUI key.

## See also

- [[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]] —
  a stop is an escape with an owner; the two cells, sliced waits, the
  collector as the owner of its stages' nodes.
- [[decisions/261016_a-cleanup-is-shielded-within-its-grace|a-cleanup-is-shielded-within-its-grace]] —
  the shield and its budget from the strike.
- [[decisions/260706_signals-are-causes|signals-are-causes]] — the collapse of
  signal delivery onto the scope tree: `Terminate`, the scope-only `check`, the
  one wait loop.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] — the
  root/foreground split and the `CancelCause` order.
- [[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]] —
  why the user interrupt writes a flag, not a counter.
- [[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] — exarch's shared
  per-agent token and its exchange-boundary reset.
- [[decisions/260504_hot-path-cancellation|hot-path-cancellation]] — the original
  cooperative-poll insight.
- [[decisions/260726_cancel-is-a-join|cancel-is-a-join]] — why the handler
  contributes an element instead of aliasing one; the folding it introduced has
  since given way to forwarding as `Control`.
- [[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]] — why the
  interrupt is time-indexed and what the run door's nesting fixed; the
  per-frame birth instants went with the folding, and a per-dispatch one
  returned at the front-end (`ControlSender::aim`).
- [[internals/output-capture-and-detachment|output-capture-and-detachment]] and
  [[internals/pipeline-execution|pipeline-execution]] — the foreground-deadline and
  group-teardown paths that read the scope.
- [[map/core/io-process|io-process]] (signals, process groups),
  [[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]],
  [[decisions/260905_one-delivery-path|one-delivery-path]] (one signal per
  process, through the collector),
  [[map/exarch/agent|agent]] (the attend loop the token wraps),
  and `core/src/process/signal.rs` itself.
