---
verified_at_commit: 99226d37
verified_at_date: 2026-09-05
anchors: [ESCALATION, CancelScope, CancelCause, Terminate, DurableRoot, ForegroundScope, Hears, request_foreground_cancel, request_root_cancel, CLOCK, STAMPED, REQUESTED_ROOT, Mooring, run_under, ChromeKind, Block::is_error, Shell::face_signals, Shell::join_session, Shell::cancel_handle, interrupt_handler, sigint_handler, sigquit_handler, grace_signal, process::check, RunningChild::wait, watch_cancel, escalation_pending]
---

# Cancellation

**Stopping in-flight work is one delivery mechanism — a *cooperative,
cause-bearing scope tree* that asks the evaluator to unwind at its next poll
point — backed by an *escalation ladder* that forces an exit when a user
insists.** A signal handler or a TUI input thread holds neither a `Shell` nor a
scope, so it *contributes a cause* to one of two process-lifetime **ambient
causes** that facing scopes fold into their join; the platform handlers
*translate* each delivered signal into such a contribution
([[decisions/260706_signals-are-causes|signals-are-causes]],
[[decisions/260726_cancel-is-a-join|cancel-is-a-join]],
[[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]). Both live in
`core/src/process/signal.rs` (see [[map/core/io-process|io-process]]); the
gestures that drive them differ per host.

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

**The force-exit floor is reachable only in non-interactive paths.** The `ral`
batch launcher binds SIGINT to `handler` (`main.rs`, `install_handlers`); the
interactive REPL rebinds SIGINT to the non-escalating `interrupt_handler`
(below), which never touches the ladder. So repeated Ctrl-C at an interactive prompt is cooperative, never a
hard kill — the escalation belongs to batch scripts, to external SIGTERM/SIGHUP,
and to exarch's async signal forward.

This is the
[[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
discipline taken to its end state: the user-facing interrupt writes a cause,
and *only* a real delivered signal walks the ladder.

## The scope tree and its cause lattice

A `CancelScope` is a node in a tree of `Arc`-linked `AtomicU8` flags. **Its
cancellation is a *join*:**

    cancelled(s) = ⨆ over chain(s) of ( flag(n) ⊔ ambient(n.hears) )

- **`cancel(cause)`** is a `fetch_max` — cancellation is one-way and *monotone*: a
  later, weaker cause can never mask a stronger one already in force.
- **One private `fold`** computes the join; `is_cancelled` and `cause` are its
  only callers, and every part it reads is private to `cancel.rs`, so no
  observer can see a cancellation except as the whole join. No mutex, no
  allocation — a handful of atomic loads per ancestor.
- The cause is an escalation order, `CancelCause`:

  | cause | value | meaning | who raises it |
  |---|---|---|---|
  | `Interrupt` | 1 | user asked the foreground to stop | Ctrl-C / Esc / batch SIGINT |
  | `Explicit` | 2 | a targeted worker teardown | `cancel <handle>`, `race` loser |
  | `Deadline` | 3 | a wall-clock / lifetime ceiling expired | `process::deadline` |
  | `Terminate` | 4 | the process was asked to shut down | SIGTERM / SIGHUP |
  | `RootAbort` | 5 | the session root is being reaped | Ctrl-`\` |

`check` maps the strongest cause to `CancelCause::message` and
`CancelCause::exit_code` — the one vocabulary every poll point shares:
`"interrupted"`, `"cancelled"`, `"timed out"`, `"aborted"` at status 130,
`"terminated"` at status 143 (`128 + SIGTERM`, what a supervisor that
SIGTERMed the process expects to read back).

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
- Children are minted by two constructors that also fix which signals reach
  them: `DurableRoot::foreground` (`Shell::dispatch`, per run) and
  `DurableRoot::worker` (shell init's boot frame, `spawn_thread`'s detached
  worker).
- **The tree is the runs' dynamic extent.** `foreground` nests each entry under
  the frame the run door displaces, not beside it under the root, so a nested
  run observes what encloses it — its outer run's interrupt, and its outer
  run's wall elapsing
  ([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]).
  **Pipelines mint no scope of their own** — they are bounded by
  the foreground scope they run under and by `CollectState::drop`, which kills
  whatever it still holds unobserved: the owned pgid, or each live external by
  pid (see [[internals/pipeline-execution|pipeline-execution]]).

## The ambient causes

A signal handler must not lock and cannot hold a `CancelScope` by value, so it
raises a cause on a process-lifetime `static` and lets the tree read it. The
two are different kinds of proposition, and `Hears` — fixed at mint, one
variant per kind — says which a node folds
([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]).

- **Shutdown is absolute.** `request_root_cancel(cause)` is one `fetch_max` on
  `REQUESTED_ROOT` — the *exact* store `scope.cancel(cause)` performs. Once
  raised it holds for every observer forever, so a SIGTERM delivered while the
  session is idle latches and the REPL reads it at its next prompt boundary.
- **An interrupt is temporal.** `request_foreground_cancel(cause)` ticks
  `CLOCK` and `fetch_max`es the new instant onto `STAMPED[cause]`; a foreground
  frame records its birth instant at mint and observes exactly the causes
  stamped after it. Two lock-free read-modify-writes, no allocation, no
  `unsafe`. Nothing is ever reset: a Ctrl-C for a settled command is older than
  the next command's frame, so the next run and the prompt after it are born
  clean.
- **Sharing, not shadowing.** A nested run reads its outer run's interrupt
  through the frame it nests under, so a Ctrl-C mid-nest unwinds the whole
  nest, as a POSIX shell's does.
- **`Shell::face_signals`** re-mints a session's `DurableRoot` folding
  `REQUESTED_ROOT`; its run doors then stamp each foreground frame with a birth
  instant, *because* the root faces. A forked session (`Shell::fork_session` —
  exarch's sub-agents) is deaf to both, and its host cancels it through a
  clonable handle on its durable root (`Shell::cancel_handle`)
  ([[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]).
- **A detached worker folds only the root cause**, through its parent: a
  SIGTERM reaches it, a Ctrl-C cannot — no node on its chain carries a birth
  instant, so the watermark is unreadable from it.
- **An aside is inside the session.** A second `Shell` a host runs beside its
  session — the REPL's hook shell, which evaluates arbitrary plugin code during
  readline — *shares* the session's `DurableRoot` (`Shell::join_session`), so a
  `cancel_handle` cancel reaches it. A Ctrl-C struck during a hook is younger
  than the hook's frame and unwinds it; one aimed at a command already in
  flight is older than every frame the aside will mint, so the aside can
  neither absorb it nor keep it from the run it was aimed at.

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
and `grace_signal(cause)` (`process/signal/unix.rs`) is the one table that
directs it — read by `RunningChild::terminate` and by the pipeline
collector's `cancel_all` alike:

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

Every external wait goes through this one `recv`/`terminate` shape — the
interactive REPL foreground included. ral does not suspend
([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]): a stop
never reaches this channel at all, since the reaper answers it with
`SIGCONT` on its own before any subscriber sees one — a stop is never itself
a cause for teardown. A foreground external still gets its Ctrl-C from the
kernel directly — it owns the terminal — but a SIGTERM delivered to *ral*
now preempts even that wait through the root cause.

## The gestures, per host

The same two mechanisms are driven by different keys on different surfaces.

| gesture | surface | what fires | effect |
|---|---|---|---|
| **Ctrl-C** | ral REPL, mid-eval | SIGINT → `interrupt_handler` | `request_foreground_cancel(Interrupt)` and nothing else — a pipeline's externals hear it through the collector; **counter untouched** |
| **Ctrl-C** | ral REPL, idle prompt | line editor reads it as a byte | abandons the partial buffer, `process::clear()`; no signal |
| **Ctrl-`\`** | ral REPL | SIGQUIT → `sigquit_handler` | `request_root_cancel(RootAbort)` — reaps foreground *and* every detached worker, latching if idle; the REPL loop observes the sticky root and exits |
| **Ctrl-C** | ral batch / `-c` | SIGINT → `handler` | `request_foreground_cancel(Interrupt)` + ladder `+1`; third press `_exit`s |
| **SIGTERM / SIGHUP** | any ral host | `handler` (term disposition) | `request_root_cancel(Terminate)` — foreground and detached workers unwind, externals torn down SIGTERM-first, exit 143; ladder `+1`, third delivery `_exit`s |
| **Ctrl-C / Esc** | exarch TUI, active exchange | `Agent::interrupt` on the focused agent (reached through that tab's own `Weak`); the trunk also `cancel::raise_interrupt` | cancels the focused agent's `Token` and the scope its interrupt target holds; on the trunk, additionally the published `Token`, `interrupt_foreground_child`, `request_foreground_cancel(Interrupt)` |
| **Ctrl-C / Ctrl-D** | exarch TUI, idle prompt | key table → quit | drops the TUI guard; no cancellation |
| **Ctrl-C / Ctrl-D / Esc** | exarch TUI overlay | key table → close overlay | returns to the underlying prompt / exchange; no root cancel |
| **async SIGINT** | exarch | `chained` handler | cancels the `Token`, then forwards into ral's non-escalating `interrupt_handler` |
| **async SIGTERM / SIGHUP** | exarch | `chained` handler | cancels the `Token`, then forwards into ral's `handler` → root `Terminate` + ladder |

### ral interactive signal dispositions

`boot::setup_signals` (`ral/src/repl/session/boot.rs`)
fixes the interactive dispositions:

- **SIGINT → `interrupt_handler`** (the `sigint_handler` it names). It raises
  the foreground interrupt and does nothing else — no delivery of its own.
  ral's own delivery to a pipeline's processes is the cancel tree alone: the
  foreground scope reaches the collector's `watch_cancel`, which posts
  `Event::Cancelled` and tears the group down with exactly one grace signal per
  process ([[internals/pipeline-execution|pipeline-execution]],
  [[decisions/260905_one-delivery-path|one-delivery-path]]). A foreground
  pipeline's externals usually hear the kernel's own copy first, delivered by
  the tty to the pgid that owns the terminal, and the anchor witnesses that so
  teardown does not re-send it. Raised while idle, the cause is older than
  every frame still to be born, so the next run never sees it — and a detached
  worker, carrying no birth instant at all, is spared outright.
- **SIGQUIT → `sigquit_handler`**, the louder "reap everything" gesture
  ([[decisions/260629_agent-binding-reaping|agent-binding-reaping]] keeps it as
  *cancellation*, never deletion). It is a cooperative `request_root_cancel`, not
  the default core-dump — so it satisfies "Ctrl-`\` must not core-dump the shell"
  *by reaping*, not by ignoring. (A prior `boot` line re-bound SIGQUIT to
  `SIG_IGN` immediately after `jobs::setup_signals` installed the handler, leaving
  Ctrl-`\` dead in the REPL against the ADR's shipped intent; the override is
  removed.)
- **SIGTERM/SIGHUP → `handler`** — translates to a root `Terminate` (the whole
  session unwinds, the REPL loop exits 143) and walks the escalation ladder;
  **SIGTSTP/SIGTTOU/SIGTTIN/SIGPIPE → `SIG_IGN`** — the shell answers a stop
  reaching one of its own children through `waitpid`/`SIGCONT` rather than
  ever being stopped itself, and rewrites terminal state without being
  stopped ([[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]]).

### exarch: the chained handler and the per-agent token

exarch layers a *per-agent* cancellation `Token` over ral's machinery
([[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]).

- **`Token`** is an `Arc<AtomicU8>` carrying a `CancelCause` (`0` while
  uncancelled), one sticky token per agent for its whole life; the attend loop
  threads clones through `deliberate`/`run_batch`/tools, so cancelling any share
  halts that agent's exchange (provider streaming, invoked tools). The trunk's
  token flag is published into exarch's own `CURRENT` slot — still the aliased
  pointer ral has retired ([[decisions/260726_cancel-is-a-join|cancel-is-a-join]]) —
  and a genuine exchange boundary `Token::reset`s the flag, so a prior
  exchange's Esc never bleeds into the next.
- **The tree cascade is two-layer.** `Agent::cancel_tree` (behind
  `` agents `cancel ``, the per-agent idle lease, and the
  `/clear`/`reply` reaps) cancels each descendant's `Token` *and* its own
  session's `DurableRoot` (`Shell::cancel_handle`, held on the `Agent` itself
  as `reach: EvalReach`). The
  token stops the attend loop between steps; the root cancel unwinds a `ral` eval
  already in flight at the evaluator's poll points — without it, a cancelled
  agent would grind to its tool's `timeout_secs` wall before noticing. The
  trunk's `Agent` carries an *interrupt-only* reach:
  `EvalReach::interrupt_only` clears its `eval_root` to `None` at construction,
  so a `terminate` there degrades to the `Token` alone and can never
  poison the one `Shell::face_signals` session the process runs on — a
  captured root would also go stale at the next `/clear`, which rebuilds the
  trunk's shell in place while an agent's reach is fixed once, at birth. What the
  ambient path still uniquely covers on the trunk: the SIGINT re-created for a
  foreground external child, and the ambient foreground stamp itself, which
  needs no dispatch handle to land and so reaches a foreground run the
  transport never dispatched
  ([[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]).
- **A cancel may precede the run it names.** Both eval-layer channels into an
  identity transport — a `Control::Cancel` naming a dispatch id, and an
  observing host's `EvalReach::Identity::interrupt` — land on the scope
  `IdentityTransport::dispatch` mints *ahead of the engine lock*, never on the
  run's own frame. That ordering is the whole point: a dispatch parked on the
  lock has no frame yet, and the cell that named the last run would answer for
  it. Cancellation being sticky and folded live along the chain, the frame is
  born a descendant of an already-cancelled scope and unwinds at its first poll
  point. A cell published only once the frame exists
  loses exactly the cancels raised in that window; the run then survives its
  interrupt, and only the cooperative `Token` — read between steps — ends the
  exchange, a tool's whole `timeout_secs` later.
- **`install`** chains ral's `term_handler`: the exarch handler `raise`s the token
  then forwards into ral's disposition, so the root-`Terminate` translation and
  the escalation ladder survive. Install order matters — ral's handler first,
  then exarch's chain — and `bootstrap::boot_shell` re-establishes it after
  every `/clear` rebuild.
- Raw mode disables `ISIG`, so a TUI keystroke is *not* a kernel signal. The TUI's
  key table (`exarch/src/tui/tui_loop.rs`) separates UI shape from cancellation:
  idle Ctrl-C/Ctrl-D quit, overlays close, and only active-exchange Ctrl-C/Esc
  route to the focused agent's own `Agent::interrupt` — every tab, the trunk included.
  The trunk's tab additionally raises `raise_interrupt`, since nothing else
  delivers the foreground external child's SIGINT or stamps the ambient
  foreground cause. `deliver_interrupt` re-creates the SIGINT the kernel would
  have sent a foreground *external* child via `interrupt_foreground_child`
  (Windows re-injects `CTRL_C_EVENT`).
- A cancelled turn is a distinct TUI `ChromeKind::Cancelled`: the rail maps it
  to the error `╳` so the broken-off work is visible, while `Block::is_error`
  still matches only `ChromeKind::Error`, keeping the matrix's failure cell for
  actual failures.

## Why interactive Ctrl-C cannot force-exit

A deliberate asymmetry worth stating plainly: **the third-signal `_exit` floor is
unreachable from an interactive prompt.** Interactive SIGINT goes to
`interrupt_handler`, which never ticks the ladder; the TUI's active-exchange
Ctrl-C goes to
`Agent::interrupt` (and, on the trunk, also `raise_interrupt`), neither
of which ever touches the ladder. Repeated presses re-write the same cause
(`fetch_max`), never escalate. The hard
floor exists for batch scripts (`handler`), for external SIGTERM/SIGHUP, and for
an async signal exarch forwards into ral. Interactive cancellation is cooperative
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
  contributes an element instead of aliasing one, and what routing by minting
  deletes.
- [[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]] — why the
  interrupt is time-indexed, what the run door's nesting fixed, and the
  authority apparatus that dissolved with the spend.
- [[internals/output-capture-and-detachment|output-capture-and-detachment]] and
  [[internals/pipeline-execution|pipeline-execution]] — the foreground-deadline and
  group-teardown paths that read the scope.
- [[map/core/io-process|io-process]] (signals, process groups),
  [[decisions/260903_ral-does-not-suspend|ral-does-not-suspend]],
  [[decisions/260905_one-delivery-path|one-delivery-path]] (one signal per
  process, through the collector),
  [[map/exarch/agent|agent]] (the attend loop the token wraps),
  and `core/src/process/signal.rs` itself.
