---
verified_at_commit: bbca4f4
verified_at_date: 2026-07-04
anchors: [ESCALATION, CancelScope, CancelCause, Terminate, DurableRoot, ForegroundScope, request_foreground_cancel, request_root_cancel, publishes_signal_slots, Shell::cancel_handle, sigint_relay, sigquit_handler, process::check, RunningChild::wait, escalation_pending]
---

# Cancellation

**Stopping in-flight work is one delivery mechanism — a *cooperative,
cause-bearing scope tree* that asks the evaluator to unwind at its next poll
point — backed by an *escalation ladder* that forces an exit when a user
insists.** A signal handler or a TUI input thread holds neither a `Shell` nor a
scope, so process-global *slots* bridge the async edge to the live tree with
async-signal-safe atomics; the platform handlers *translate* each delivered
signal into a cause on those slots
([[decisions/260706_signals-are-causes|signals-are-causes]]). Both live in
`core/src/process/signal.rs` (see [[map/core/io-process|io-process]]); the
gestures that drive them differ per host.

The two pieces answer different questions:

- **The scope tree** (`CancelScope`) answers *"which subtree should unwind, and
  why?"* — a structured-concurrency primitive that names a *cause* and reaches
  exactly the workers that inherited the cancelled scope. It is the only thing
  `process::check(shell)` polls.
- **The ladder** (`ESCALATION: AtomicU8`) answers *"is the user escalating
  toward kill?"* — the third delivery forces `_exit(128 + sig)`. It is a blunt,
  host-agnostic floor for a process whose cooperative delivery is wedged, never
  a delivery mechanism itself.

## The escalation ladder

The platform handler `fetch_add`s the ladder on every delivered termination
signal; the third hit calls `libc::_exit(128 + sig)` — bypassing `atexit` so a
wedged process always dies. Nothing else reads it for control flow: `clear()`
resets it at acknowledgment boundaries (a fresh prompt, a turn compile, a
session reboot), and `escalation_pending()` exposes it for observability only.

**The force-exit floor is reachable only in non-interactive paths.** The `ral`
batch launcher binds SIGINT to `handler` (`main.rs`, `install_handlers`); the
interactive REPL rebinds SIGINT to the *relay* (below), which never touches the
ladder. So repeated Ctrl-C at an interactive prompt is cooperative, never a
hard kill — the escalation belongs to batch scripts, to external SIGTERM/SIGHUP,
and to exarch's async signal forward.

This is the
[[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
discipline taken to its end state: the user-facing interrupt writes a cause,
and *only* a real delivered signal walks the ladder.

## The scope tree and its cause lattice

A `CancelScope` is a node in a tree of `Arc`-linked `AtomicU8` flags. A scope is
cancelled iff its own flag or any ancestor's flag is set.

- **`cancel(cause)`** is a `fetch_max` — cancellation is one-way and *monotone*: a
  later, weaker cause can never mask a stronger one already in force.
- **`is_cancelled`** walks the parent chain reading flags; **`cause`** walks it
  taking the maximum. No mutex, no allocation — one `AtomicU8::load` per ancestor.
- The cause is an escalation order, `CancelCause`:

  | cause | value | meaning | who raises it |
  |---|---|---|---|
  | `Interrupt` | 1 | user asked the foreground to stop | Ctrl-C / Esc / batch SIGINT |
  | `Explicit` | 2 | a targeted worker teardown | `cancel <handle>`, `race` loser |
  | `Deadline` | 3 | a wall-clock / lifetime ceiling expired | `process::reaper` |
  | `Terminate` | 4 | the process was asked to shut down | SIGTERM / SIGHUP |
  | `RootAbort` | 5 | the session root is being reaped | Ctrl-`\` |

`check` maps the strongest cause to `CancelCause::message` and
`CancelCause::exit_code` — the one vocabulary every poll point shares:
`"interrupted"`, `"cancelled"`, `"timed out"`, `"aborted"` at status 130,
`"terminated"` at status 143 (`128 + SIGTERM`, what a supervisor that
SIGTERMed the process expects to read back).

### Two typed scopes name the one invariant

The tree's load-bearing rule — *a turn's foreground scope is always a descendant
of the session's durable root* — is spelled in the type system, not left to
discipline ([[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]]).

- **`DurableRoot`** (`shell.session.root`) is minted once per `Shell`. Detached
  workers — `spawn`, `&`, `watch` — parent under it, so a *foreground* cancel
  never reaches them ([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]]).
- **`ForegroundScope`** (`shell.turn.cancel`) is the turn's work scope. It can be
  minted *only* from a `DurableRoot` (or by nesting another foreground), so an
  unrelated root can never be installed as a foreground by accident.
- Children are minted at exactly four sites: shell init (`root.child()` →
  foreground), `build_turn` (`foreground.child()` per nested turn), the
  concurrency worker (`root.child()` — root-parented, *not* foreground), and the
  IPC inherit path. **Pipelines mint no scope of their own** — they are bounded by
  the foreground scope they run under and by `PipelineGroup::Drop`, which group-
  SIGKILLs on teardown (see [[internals/pipeline-execution|pipeline-execution]]).

## The signal-reachable slots

A signal handler must not lock and cannot hold a `CancelScope` by value. Two
process-global `AtomicPtr<AtomicU8>` slots publish a *borrowed pointer* into the
live scope's flag for the async edge to set.

- **`FOREGROUND_SCOPE`** and **`DURABLE_ROOT_SCOPE`** are published together, for
  the turn's whole extent, by `TurnGuard::install` (`core/src/turn.rs`) when
  `eval_turn` swaps a `TurnState` in. Publication is a *swap*, not a store, so a
  re-entrant turn nests its scope above the outer one and reveals it again on drop
  ([[decisions/260617_turn-local-state|turn-local-state]]).
- **At most one session per process publishes** — the *signal-facing* one, marked
  by `SessionState::publishes_signal_slots`. The swap/restore discipline is LIFO
  on a single thread, which concurrent sessions would violate; and a signal must
  target the primary session's turn, never whichever session dispatched last. A
  forked session (`Shell::fork_session` — exarch's sub-agents) never publishes;
  its host cancels it through a clonable handle on its durable root
  (`Shell::cancel_handle`)
  ([[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]).
- **`request_foreground_cancel(cause)`** / **`request_root_cancel(cause)`** load
  the slot and `fetch_max` the cause onto the borrowed flag — the *exact* store
  `scope.cancel(cause)` performs, and itself async-signal-safe. A null slot
  (between turns) makes the request a no-op, so an idle Ctrl-C or Ctrl-`\` touches
  nothing.
- Drop order is the safety argument: `TurnGuard` declares the slot guards *before*
  the displaced frame, so they un-publish before the scope `Arc` they borrow can
  free — a slot never points at a freed flag.

The slots are the seam the
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] ADR calls the
"cancel-translation slots": the semantic collapse is onto the scope tree, never
into the signal handler.

## Where cancellation is observed: poll points

A cancel is a *request*; nothing stops until the evaluator next polls. The poll is
`process::check(shell)`, called at:

- the **trampoline** (`evaluator/comp.rs`, `evaluator/trampoline.rs`) — every tail
  step, so any loop of `ral` calls is preemptible (the original
  [[decisions/260504_hot-path-cancellation|hot-path-cancellation]] insight);
- the **iterating builtins** (`builtins/collections.rs`, `builtins/concurrency.rs`)
  — `map`/`filter`/`each` and the worker-join loops poll between elements;
- **pipeline launch** (`runtime/pipeline.rs`, `runtime/pipeline/launch.rs`) —
  before and between stage spawns.

A computation that never reaches a poll point (a tight Rust loop inside one
builtin) is not interruptible by the scope path — the contract is *cooperative*.

### External children: teardown by cause

A blocked `waitpid` does not consult the scope, so `RunningChild::wait`
(`runtime/command/child.rs`) wraps it in a cancel-aware poll loop with
exponential backoff (5 ms → 100 ms cap). On each iteration it `try_wait`s
(WUNTRACED, so a SIGSTOP'd child is seen, not spun on) and reads
`self.cancel.cause()`. The teardown is *cause-directed*:

- **`Interrupt`** → SIGINT-first, a 500 ms grace, then a group SIGKILL — a child
  that traps SIGINT still dies, and its grandchildren with it.
- **`Explicit` / `Deadline` / `Terminate`** → SIGTERM-first with the same grace
  then group SIGKILL — decisive, without pretending to be a user keystroke; a
  `Terminate` hands the tree the very signal the supervisor sent ral.
- **`RootAbort`** → an immediate group SIGKILL, no grace.

Every external wait goes through this one loop — the interactive REPL
foreground included (`park_on_stop = true` there makes a SIGSTOP *classify* as
a parked job instead of a kill-and-reap; it no longer selects a different,
blocking wait). A foreground external still gets its Ctrl-C from the kernel
directly — it owns the terminal (see [[map/repl/jobs|jobs]]) — but a SIGTERM
delivered to *ral* now preempts even that wait through the root cause.

## The gestures, per host

The same two mechanisms are driven by different keys on different surfaces.

| gesture | surface | what fires | effect |
|---|---|---|---|
| **Ctrl-C** | ral REPL, mid-eval | SIGINT → `sigint_relay` | `request_foreground_cancel(Interrupt)` + relay SIGINT to external pgids; **counter untouched** |
| **Ctrl-C** | ral REPL, idle prompt | line editor reads it as a byte | abandons the partial buffer, `process::clear()`; no signal |
| **Ctrl-`\`** | ral REPL | SIGQUIT → `sigquit_handler` | `request_root_cancel(RootAbort)` — reaps foreground *and* every detached worker; the REPL loop observes the sticky root and exits |
| **Ctrl-C** | ral batch / `-c` | SIGINT → `handler` | `request_foreground_cancel(Interrupt)` + ladder `+1`; third press `_exit`s |
| **SIGTERM / SIGHUP** | any ral host | `handler` (term disposition) | `request_root_cancel(Terminate)` — foreground and detached workers unwind, externals torn down SIGTERM-first, exit 143; ladder `+1`, third delivery `_exit`s |
| **Ctrl-C / Esc** | exarch TUI, active turn | `cancel::raise_interrupt` | cancels the per-turn `Token`, `interrupt_foreground_child`, `request_foreground_cancel(Interrupt)` |
| **Ctrl-C / Ctrl-D** | exarch TUI, idle prompt | key table → quit | drops the TUI guard; no cancellation |
| **Ctrl-C / Ctrl-D / Esc** | exarch TUI overlay | key table → close overlay | returns to the underlying prompt / turn; no root cancel |
| **async SIGINT** | exarch | `chained` handler | cancels the `Token`, then forwards into ral's non-escalating `sigint_relay` |
| **async SIGTERM / SIGHUP** | exarch | `chained` handler | cancels the `Token`, then forwards into ral's `handler` → root `Terminate` + ladder |

### ral interactive signal dispositions

`jobs::setup_signals` then `boot::setup_signals` (`ral/src/repl/session/boot.rs`)
fix the interactive dispositions:

- **SIGINT → relay** (`sigint_relay`). The relay keeps the controlling tty with
  the shell while a *mixed* pipeline (internal threads + external processes) runs,
  fanning SIGINT out to up to eight active external pgids via the `RELAY_PGIDS`
  slot array (`PipelineRelay` RAII). It *also* `request_foreground_cancel`s so an
  in-process foreground computation unwinds. It is a no-op when idle.
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
  **SIGTSTP/SIGTTOU/SIGTTIN/SIGPIPE → `SIG_IGN`** (the shell drives job control by
  `waitpid` and rewrites terminal state without being stopped).

### exarch: the chained handler and the per-turn token

exarch layers a *per-root-turn* cancellation `Token` over ral's machinery
([[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]).

- **`Token`** is an `Arc<AtomicBool>`, one sticky token per agent for its whole
  life; the drive loop threads clones through `apply`/dispatch/tools, so
  cancelling any share halts that agent's turn (provider streaming, staged
  tools). The trunk's token flag is published into exarch's own `CURRENT` slot —
  the same lock-free pattern as ral's — and a genuine turn boundary
  `Token::reset`s the flag, so a prior turn's Esc never bleeds into the next.
- **The registry cascade is two-layer.** `AgentRegistry::cancel` (behind
  `agent-cancel`, the per-agent idle lease, and the
  `/clear`/`reply` reaps) cancels each descendant's `Token` *and* its own
  session's `DurableRoot` (`Shell::cancel_handle`, held per registry entry). The
  token stops the drive loop between steps; the root cancel unwinds a `ral` eval
  already in flight at the evaluator's poll points — without it, a cancelled
  agent would grind to its tool's `timeout_secs` wall before noticing. The trunk
  carries no eval-root handle: its session outlives any cancel, and Esc reaches
  its turn through the published foreground slot instead
  ([[decisions/260704_per-agent-eval-cancel|per-agent-eval-cancel]]).
- **`install`** chains ral's `term_handler`: the exarch handler `raise`s the token
  then forwards into ral's disposition, so the root-`Terminate` translation and
  the escalation ladder survive. Install order matters — ral's handler first,
  then exarch's chain — and `bootstrap::boot_shell` re-establishes it after
  every `/clear` rebuild.
- Raw mode disables `ISIG`, so a TUI keystroke is *not* a kernel signal. The TUI's
  key table (`exarch/src/tui.rs`) separates UI shape from cancellation: idle
  Ctrl-C/Ctrl-D quit, overlays close, and only active-turn Ctrl-C/Esc route to
  `raise_interrupt`. `deliver_interrupt` re-creates the SIGINT the kernel would
  have sent a foreground *external* child via `interrupt_foreground_child`
  (Windows re-injects `CTRL_C_EVENT`).

## Why interactive Ctrl-C cannot force-exit

A deliberate asymmetry worth stating plainly: **the third-signal `_exit` floor is
unreachable from an interactive prompt.** Interactive SIGINT goes to the relay,
which never ticks the ladder; the TUI's active-turn Ctrl-C goes to
`raise_interrupt`, which writes a flag and a cause but never the ladder.
Repeated presses re-write the same cause (`fetch_max`), never escalate. The hard
floor exists for batch scripts (`handler`), for external SIGTERM/SIGHUP, and for
an async signal exarch forwards into ral. Interactive cancellation is cooperative
by construction; the root-reap gesture is REPL Ctrl-`\`, not a TUI key.

## See also

- [[decisions/260706_signals-are-causes|signals-are-causes]] — the collapse of
  signal delivery onto the scope tree: `Terminate`, the scope-only `check`, the
  one wait loop.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] — the
  root/foreground split, the `CancelCause` order, and the per-turn cancel slots.
- [[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]] —
  why the user interrupt writes a flag, not a counter.
- [[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]] — exarch's shared
  root-turn token and its mint-as-reset.
- [[decisions/260504_hot-path-cancellation|hot-path-cancellation]] — the original
  cooperative-poll insight.
- [[internals/output-capture-and-detachment|output-capture-and-detachment]] and
  [[internals/pipeline-execution|pipeline-execution]] — the foreground-deadline and
  group-teardown paths that read the scope.
- [[map/core/io-process|io-process]] (signals, process groups), [[map/repl/jobs|jobs]]
  (relay, fg/bg), [[map/exarch/agent|agent]] (the turn loop the token wraps),
  and `core/src/process/signal.rs` itself.
