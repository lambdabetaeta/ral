---
verified_at_commit: afc5d952
verified_at_date: 2026-09-23
anchors: [ESCALATION, forward_ambient, Ambient, ControlSender::forward_signals, CancelScope, CancelCause, Terminate, DurableRoot, ForegroundScope, request_interrupt, request_root_cancel, INTERRUPTS, REQUESTED_ROOT, Mooring, run_under, Chrome, Scrollback::last_is_error, Shell::join_session, Shell::cancel_handle, interrupt_handler, sigint_handler, sigquit_handler, grace_signal, signals_of, gesture_signal, GESTURES, gesture, TerminalLoan, TerminalLoan::hear, TerminalLoan::reclaim, WaitOutcome::death, Status::code, ChildEnd, WaitOutcome::classify, KILL_EXIT_CODE, process::check, RunningChild::wait, watch_cancel, Membership::owes, break_pipeline_group, escalation_pending]
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
  `process::check(mooring)` polls.
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
(below), which never touches the ladder. So repeated Ctrl-C at an interactive prompt is cooperative, never a
hard kill — the escalation belongs to batch scripts and to external
SIGTERM/SIGHUP.

This is the
[[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
discipline taken to its end state: the user-facing interrupt writes a cause,
and *only* a real delivered signal walks the ladder.

## The scope tree and its cause lattice

A `CancelScope` is a node in a tree of `Arc`-linked `AtomicU8` flags. **Its
cancellation is a *join*:**

    cancelled(s) = ⨆ over chain(s) of flag(n)

- **`cancel(cause)`** is a `fetch_max` — cancellation is one-way and *monotone*: a
  later, weaker cause can never mask a stronger one already in force.
- **One private `fold`** computes the join; `is_cancelled` and `cause` are its
  only callers, and every part it reads is private to `cancel.rs`, so no
  observer can see a cancellation except as the whole join. No mutex, no
  allocation — a handful of atomic loads per ancestor.
- The cause is an escalation order, `CancelCause`:

  | cause | value | meaning | who raises it |
  |---|---|---|---|
  | `ReaderGone` | 1 | a pipeline stage's reader has ended | the pipeline collector |
  | `Interrupt` | 2 | user asked the foreground to stop | Ctrl-C / Esc / batch SIGINT |
  | `Explicit` | 3 | a targeted worker teardown | `cancel <handle>`, `race` loser |
  | `Deadline` | 4 | a wall-clock / lifetime ceiling expired | `process::deadline` |
  | `Terminate` | 5 | the process was asked to shut down | SIGTERM / SIGHUP |
  | `RootAbort` | 6 | the session root is being reaped | Ctrl-`\` |

`check` maps the strongest cause to `Error::cancelled(cause)`, whose message
is `CancelCause::message` — the one vocabulary every poll point shares.

### Two typed scopes name the one invariant

The tree's load-bearing rule — *a run's foreground scope is always a descendant
of the session's durable root* — is spelled in the type system, not left to
discipline ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).

- **`DurableRoot`** (`shell.session.root`) is minted once per `Shell`. Detached
  workers — `spawn`, `watch` — parent under it, so a *foreground* cancel
  never reaches them ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).
- **`ForegroundScope`** (`Mooring::cancel`) is the run's work scope. It can be
  minted *only* from a `DurableRoot` (or by nesting another foreground), so an
  unrelated root can never be installed as a foreground by accident.
- `DurableRoot::worker` mints the scopes beside a run: shell init's anchor and
  `spawn_thread`'s detached worker. A run's frame nests under the anchor, so
  cancelling it never reaches a worker, while cancelling the root reaches both.
- **The tree is the runs' dynamic extent.** `Shell::dispatch` nests each entry
  under the frame the run door displaces, not beside it under the root, so a
  nested run observes what encloses it — its outer run's interrupt, and its
  outer run's wall elapsing
  ([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]).
  **Pipelines mint no scope of their own** — they are bounded by
  the foreground scope they run under and by `CollectState::drop`, which kills
  whatever it still holds unobserved: the owned pgid, or each live external by
  pid (see [[internals/pipeline-execution|pipeline-execution]]).

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
  `ControlSender::forward_signals` maps an interrupt to `Control::Interrupt`,
  which strikes only the dispatch in flight, a root `RootAbort` to `Abort` and
  any other root cause to `Terminate`, which cancel the durable root. The REPL
  and batch hold one for the session, a wire engine one over its own scopes,
  and exarch forwards to its trunk's `Agent` (`cancel::face`). So a process
  hosting many engines — exarch, a test binary — never has one signal reach
  them all.
- **Sharing, not shadowing.** A nested run nests under the frame the
  interrupt strikes, so a Ctrl-C mid-nest unwinds the whole nest, as a POSIX
  shell's does.
- **A detached worker hears only the root**: `Terminate` reaches it, an
  interrupt, striking a run's frame, cannot.
- **A forked session** (`Shell::fork_session` — exarch's sub-agents) hears no
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

## Where cancellation is observed: poll points

A cancel is a *request*; nothing stops until the evaluator next polls. The poll is
`process::check(mooring)`, called at:

- the **machine's step arms** (`evaluator/machine.rs`) — the β-step, `Bind`,
  `App`, `Rec`, `Source`, and the exec step each poll, so any loop of `ral`
  calls is preemptible (the original
  [[decisions/260504_hot-path-cancellation|hot-path-cancellation]] insight);
  a `?` chain's advance from one arm to the next is nested `try` applying a
  handler thunk, so it polls at the same β-step;
- the **iterating builtins** (`builtins/collections.rs`, `builtins/concurrency.rs`)
  — `map`/`filter`/`each` and the worker-join loops poll between elements;
- **pipeline launch** (`runtime/pipeline.rs`, `runtime/pipeline/launch.rs`) —
  before and between stage spawns.

A computation that never reaches a poll point (a tight Rust loop inside one
builtin) is not interruptible by the scope path — the contract is *cooperative*.

### External children: teardown by cause

A blocked wait does not consult the scope, so nothing here polls at all: the
process-wide reaper ([[map/core/io-process|io-process]]) posts a child's exit
straight onto `RunningChild::wait`'s own channel, and `watch_cancel` posts a
cancel onto the same channel the instant the scope's cause is set — one
blocking `recv`, no interval, no backoff. The teardown is *cause-directed*,
and `grace_signal(cause)` (`process/signal/unix.rs`, and its Windows twin)
is the one table that directs it — read by `RunningChild::terminate` and by
the pipeline collector's `cancel_all` alike:

- **`Interrupt`** → SIGINT-first, a bounded `TEARDOWN_GRACE` (500 ms), then a
  group (or, ungrouped, pid-`Watch`) SIGKILL — a child that traps SIGINT
  still dies, and its grandchildren with it.
- **`Explicit` / `Deadline` / `Terminate`** → SIGTERM-first with the same
  grace then the same kill — decisive, without pretending to be a user
  keystroke; a `Terminate` hands the tree the very signal the supervisor sent
  ral.
- **`ReaderGone` / `RootAbort`** → `None`, an immediate kill with no grace. A
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
*ral* preempts even that wait through the root cause.

## One status per fact

**A run's failure is a function of its cause, never of where the cancel
landed.** A cancel that lands before a spawn meets a poll point; one that
lands after meets a torn-down child. Both doors mint the one constructor
`Status::Cancelled(cause)`, so the two cannot disagree. `Status` is a
two-level sum, each level with its own messages and hints:

    Status         = Raised(i32) | Cancelled(CancelCause) | Process(CommandFailure)
    CommandFailure = ExitCode(i32) | Signal(Signal) | Spawn(SpawnFailure)

`Status::code` is the sole home of `128 + n` and of the cause table:

| status | code |
|---|---:|
| `Raised(n)`, `ExitCode(n)` | n |
| `Signal(s)` | 128 + s |
| spawn: not found | 127 |
| spawn: any other | 126 |
| `Cancelled(Interrupt)` | 130 (`128 + SIGINT`) |
| `Cancelled(Explicit)` | 143 (`128 + SIGTERM`, what `kill` sends) |
| `Cancelled(Deadline)` | 124 (as `timeout(1)`) |
| `Cancelled(Terminate)` | 143 (`128 + SIGTERM`) |
| `Cancelled(RootAbort)` | 131 (`128 + SIGQUIT`) |
| `Cancelled(ReaderGone)` | 141 (`128 + SIGPIPE`) |

The same facts reach ral code as the error record's `reason`
(`evaluator/scope.rs`, `reason_value`), of which `status` is the projection.
`reader-gone` is among them because a `try` inside a stage body can observe
its own cut.

### Attribution

A child's death passes through `WaitOutcome::classify(cause, enveloped)`,
which yields a transient `ChildEnd`: `Failed(CommandFailure)` or
`Cancelled(CancelCause)`, and `Error::of_child` maps the second to
`Error::cancelled` exactly as a poll point does. `cause` is the strongest in
force when the death is observed, not only what ral sent.

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
| `RootAbort` | SIGKILL, SIGQUIT | `KILL_EXIT_CODE` |
| `Interrupt` | SIGINT, SIGKILL | `KILL_EXIT_CODE`, `STATUS_CONTROL_C_EXIT` |
| `Explicit`, `Deadline` | SIGTERM, SIGKILL | `KILL_EXIT_CODE`, `STATUS_CONTROL_C_EXIT` |
| `Terminate` | SIGTERM, SIGHUP, SIGKILL | `KILL_EXIT_CODE`, `STATUS_CONTROL_C_EXIT` |

**Only a terminal ral lent can report a key pressed on it.** A key is what a
terminal sends: the `GESTURES` table in `process/signal/unix.rs` — SIGINT →
`Interrupt`, SIGQUIT → `RootAbort`, SIGHUP → `Terminate`; no terminal sends
SIGTERM. Its reader `gesture` is private to the `TerminalLoan`, the one
lending of the session's `TerminalLease` to a group, for a run. While it is
lent the key reaches the tenant's group and never ral, so the loan hears it
back from the tenant — `TerminalLoan::hear` — and when the terminal returns
it strikes what it heard on the frame it was lent for, before the next poll:

- a **lone command** is heard at `reclaim`, the instant it is dead, from its
  own `death`; `command.rs` folds that cause into the one `classify` reads,
  so Ctrl-C reads `` `cancelled `interrupted `` and the run is cancelled;
- a **pipeline**'s loan is held by its collector: the anchor reports each
  signal it swallowed raw as `Event::Heard(signal)`, a key tears the group
  down with `delivered: true`, and once the anchor has ended the fold hears
  out every queued report and settles each external stage under
  `sent.max(pressed)`. The loan drops with the collector, striking the frame
  whatever the verdict — the stages may all have trapped the key.

The frame the loan strikes is always the run's own: inside a run the only
nested `ForegroundScope` is a stage thread's, and neither a stage's child
nor a stage's pipeline is lent, so no `try` stands between the strike and
the run's closing poll. What the loan cannot settle is an external inside a
*stage body*: a `Joins` member whose waiter reads its scope as the reaper
posts its death, racing the anchor's report through the collector to that
scope. Lose the race and the stage reads `` `signaled 2 `` where a direct
stage, settled under `pressed`, reads `` `cancelled `interrupted `` — the
same exit 130, and the run stops either way.

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
a `Deadline` teardown reads `timed out`, never an exit status — and with no
such cause in force it is displayed as "ended by Ctrl-C". A console is never
lent: the Windows `TerminalLoan` is uninhabited. A cancelled child carries no
hint.

## The gestures, per host

The same two mechanisms are driven by different keys on different surfaces.

| gesture | surface | what fires | effect |
|---|---|---|---|
| **Ctrl-C** | ral REPL, mid-eval | SIGINT → `interrupt_handler` | `request_interrupt()`, forwarded as `Control::Interrupt` — a pipeline's externals hear it through the collector; **counter untouched** |
| **Ctrl-C** | ral REPL, idle prompt | line editor reads it as a byte | abandons the partial buffer, `process::clear()`, and sends `Control::Interrupt`, a no-op with nothing in flight; no signal |
| **Ctrl-`\`** | ral REPL, terminal not lent | SIGQUIT → `sigquit_handler` | `request_root_cancel(RootAbort)`, forwarded as `Control::Abort` — reaps foreground *and* every detached worker, latching if idle; the REPL loop observes the sticky root and exits |
| **Ctrl-C / Ctrl-`\` / hangup** | any ral host, terminal lent to a command or pipeline | tty → the tenant's group (a hangup also reaches ral, as session leader) | the `TerminalLoan` hears it from the tenant — its death, or the anchor's `Heard` — and strikes `Interrupt` / `RootAbort` / `Terminate` on the run's frame as the terminal returns: the job ends, `try` cannot recover it, the session lives |
| **Ctrl-C** | ral batch / `-c` | SIGINT → `handler` | `request_interrupt()`, forwarded as `Control::Interrupt`, + ladder `+1`; third press `_exit`s |
| **SIGTERM / SIGHUP** | any ral host | `handler` (term disposition) | `request_root_cancel(Terminate)`, forwarded as `Control::Terminate` — foreground and detached workers unwind, externals torn down SIGTERM-first, exit 143; ladder `+1`, third delivery `_exit`s |
| **Ctrl-C / Ctrl-Break** | ral on a Windows console | console event → the `ctrlc` handler | `request_interrupt()`, forwarded as `Control::Interrupt`, + ladder `+1`; third event `ExitProcess`es with 130. No console event is sent to any group: the struck run's teardown breaks the groups it owns, and a worker's are never reached |
| **Ctrl-C / Esc** | exarch TUI, active exchange | `Agent::interrupt` on the focused agent (reached through that tab's own `Weak`); the trunk also `cancel::raise_interrupt` | cancels the focused agent's `Token` and, as `Control::Interrupt`, its dispatch in flight; on the trunk, additionally `interrupt_foreground_child` (Unix) or `request_interrupt()` (Windows) |
| **Ctrl-C / Ctrl-Break** | exarch on a Windows console | console event → `console_ctrl_handler`, ahead of ral's | `request_interrupt()`, reported handled so ral's ladder never sees it; `face` forwards it to the trunk's `Agent::interrupt` |
| **Ctrl-C / Ctrl-D** | exarch TUI, idle prompt | key table → quit | drops the TUI guard; no cancellation |
| **Ctrl-C / Ctrl-D / Esc** | exarch TUI overlay | key table → close overlay | returns to the underlying prompt / exchange; no root cancel |
| **async SIGINT** | exarch | ral's non-escalating `interrupt_handler` | forwarded by `cancel::face` to the trunk's `Agent::interrupt` |
| **async SIGTERM / SIGHUP** | exarch | ral's `handler` | forwarded to the trunk's `Agent::cancel(Terminate)` — its `Token` and `Control::Terminate`; ladder `+1` |

### ral interactive signal dispositions

`boot::setup_signals` (`ral/src/repl/session/boot.rs`)
fixes the interactive dispositions:

- **SIGINT → `interrupt_handler`** (the `sigint_handler` it names). It raises
  the interrupt and does nothing else — no delivery of its own. The session
  forwards it as `Control::Interrupt`, and ral's own delivery to a pipeline's
  processes is the cancel tree alone: the struck scope reaches the collector's `watch_cancel`, which posts
  `Event::Cancelled` and tears the group down with exactly one grace signal per
  process ([[internals/pipeline-execution|pipeline-execution]],
  [[decisions/260905_one-delivery-path|one-delivery-path]]). A foreground
  pipeline's externals usually hear the kernel's own copy first, delivered by
  the tty to the pgid the terminal is lent to; the anchor reports it and the
  pipeline's loan hears it as the key, so teardown does not re-send it.
  Raised while idle, it strikes only the settled run's dead scope, so the
  next run never sees it — and a detached worker, off every run's chain, is
  spared outright.
- **SIGQUIT → `sigquit_handler`**, the louder "reap everything" gesture
  ([[decisions/260629_agent-binding-reaping|agent-binding-reaping]] keeps it as
  *cancellation*, never deletion). It is a cooperative `request_root_cancel`, not
  the default core-dump — so it satisfies "Ctrl-`\` must not core-dump the shell"
  *by reaping*, not by ignoring.
- **SIGTERM/SIGHUP → `handler`** — translates to a root `Terminate` (the whole
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
  `bootstrap::face_process_signals` runs it once, at the process entry; each
  site that launches a process trunk holds `cancel::face` for the attend, which
  forwards the ambient causes to that trunk's `Agent`. `/clear` resets only the
  escalation ladder.
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
  per-frame birth instants went with the folding.
- [[internals/output-capture-and-detachment|output-capture-and-detachment]] and
  [[internals/pipeline-execution|pipeline-execution]] — the foreground-deadline and
  group-teardown paths that read the scope.
- [[map/core/io-process|io-process]] (signals, process groups),
  [[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]],
  [[decisions/260905_one-delivery-path|one-delivery-path]] (one signal per
  process, through the collector),
  [[map/exarch/agent|agent]] (the attend loop the token wraps),
  and `core/src/process/signal.rs` itself.
