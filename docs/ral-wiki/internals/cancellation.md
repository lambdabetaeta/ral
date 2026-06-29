---
verified_at_commit: 7950be9
verified_at_date: 2026-06-18
anchors: [SIGNAL_COUNT, CancelScope, CancelCause, DurableRoot, ForegroundScope, request_foreground_cancel, request_root_cancel, sigint_relay, sigquit_handler, process::check, RunningChild::wait]
---

# Cancellation

**Stopping in-flight work is two mechanisms wearing one module name: an
*escalating termination counter* that forces an exit when a user insists, and a
*cooperative, cause-bearing scope tree* that asks the evaluator to unwind at its
next poll point.** A signal handler or a TUI input thread holds neither a `Shell`
nor a scope, so process-global *slots* bridge the async edge to the live tree
with async-signal-safe atomics. Both live in `core/src/process/signal.rs` (see
[[map/core/io-process|io-process]]); the gestures that drive them differ per
host.

The two mechanisms answer different questions:

- **The counter** (`SIGNAL_COUNT: AtomicU8`) answers *"is the user escalating
  toward kill?"* — `0` normal, `1` interrupted, `2` again, `>= 3` force
  `_exit(128 + sig)`. It is a blunt, host-agnostic floor.
- **The scope tree** (`CancelScope`) answers *"which subtree should unwind, and
  why?"* — a structured-concurrency primitive that names a *cause* and reaches
  exactly the workers that inherited the cancelled scope.

`process::check(shell)` consults *both* on every poll; either reason unwinds the
same way, into a `Break::Error` carrying transport status **130**.

## The escalating counter

The platform handler `fetch_add`s the counter; `check` reads it; the third hit
`_exit`s.

- A real **SIGINT/SIGTERM/SIGHUP** reaching the bare `handler`
  (`signal/unix.rs`) increments the counter. The *third* delivery calls
  `libc::_exit(128 + sig)` — bypassing `atexit` so a wedged process always dies.
- `check` treats any count `>= 1` as `"interrupted"` and unwinds. `clear()`
  resets to `0`; `interrupt()` *stores* `1` (idempotent — it asks for an unwind
  without ever feeding the third-signal escalation).
- **This force-exit floor is reachable only in non-interactive paths.** The `ral`
  batch launcher binds SIGINT to `handler` (`main.rs`, `install_handlers`); the
  interactive REPL rebinds SIGINT to the *relay* (below), which never touches the
  counter. So repeated Ctrl-C at an interactive prompt is cooperative, never a
  hard kill — the escalation belongs to batch scripts and to exarch's async
  signal forward.

This is the
[[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
discipline: the user-facing interrupt writes a flag, it does not pump a counter.

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
  | `Interrupt` | 1 | user asked the foreground to stop | Ctrl-C / Esc |
  | `Explicit` | 2 | a targeted worker teardown | `cancel <handle>`, `race` loser |
  | `Deadline` | 3 | a wall-clock / lifetime ceiling expired | `process::reaper` |
  | `RootAbort` | 4 | the session root is being reaped | Ctrl-`\` |

`check` maps the strongest cause to a message — `"interrupted"`, `"cancelled"`,
`"timed out"`, `"aborted"` — all at status 130.

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
- **`Explicit` / `Deadline`** → SIGTERM-first with the same grace then group
  SIGKILL — decisive, without pretending to be a user keystroke.
- **`RootAbort`** → an immediate group SIGKILL, no grace.

The interactive REPL foreground sets `park_on_stop = true` and *skips* this loop:
there a foreground external owns the terminal and Ctrl-C is delivered to its
process group by the kernel directly (see [[map/repl/jobs|jobs]]).

## The gestures, per host

The same two mechanisms are driven by different keys on different surfaces.

| gesture | surface | what fires | effect |
|---|---|---|---|
| **Ctrl-C** | ral REPL, mid-eval | SIGINT → `sigint_relay` | `request_foreground_cancel(Interrupt)` + relay SIGINT to external pgids; **counter untouched** |
| **Ctrl-C** | ral REPL, idle prompt | line editor reads it as a byte | abandons the partial buffer, `process::clear()`; no signal |
| **Ctrl-`\`** | ral REPL | SIGQUIT → `sigquit_handler` | `request_root_cancel(RootAbort)` — reaps foreground *and* every detached worker |
| **Ctrl-C** | ral batch / `-c` | SIGINT → `handler` | counter `+1`; third press `_exit`s |
| **Ctrl-C / Esc** | exarch TUI, active turn | `cancel::raise_interrupt` | cancels the per-turn `Token`, `interrupt_foreground_child`, `request_foreground_cancel(Interrupt)` |
| **Ctrl-C / Ctrl-D** | exarch TUI, idle prompt | key table → quit | drops the TUI guard; no cancellation |
| **Ctrl-C / Ctrl-D / Esc** | exarch TUI overlay | key table → close overlay | returns to the underlying prompt / turn; no root cancel |
| **async SIGINT** | exarch | `chained` handler | cancels the `Token`, then forwards into ral's escalating `handler` |

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
- **SIGTERM/SIGHUP → `handler`** (escalating, for batch-style external kills);
  **SIGTSTP/SIGTTOU/SIGTTIN/SIGPIPE → `SIG_IGN`** (the shell drives job control by
  `waitpid` and rewrites terminal state without being stopped).

### exarch: the chained handler and the per-turn token

exarch layers a *per-root-turn* cancellation `Token` over ral's machinery
([[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]).

- **`Token`** is an `Arc<AtomicBool>`; a sub-agent shares a *clone*, so one
  active-turn Ctrl-C or Esc halts the whole call tree (provider streaming,
  staged tools, child sessions).
  The current root token's flag is published into exarch's own `CURRENT` slot —
  the same lock-free pattern as ral's — read by the provider's mid-stream cancel
  race, which holds no token.
- **`install`** chains ral's `term_handler`: the exarch handler `raise`s the token
  then forwards into ral's disposition, so SIGNAL_COUNT semantics survive. Install
  order matters — ral's handler first, then exarch's chain — and
  `bootstrap::boot_shell` re-establishes it after every `/clear` rebuild.
- Raw mode disables `ISIG`, so a TUI keystroke is *not* a kernel signal. The TUI's
  key table (`exarch/src/tui.rs`) separates UI shape from cancellation: idle
  Ctrl-C/Ctrl-D quit, overlays close, and only active-turn Ctrl-C/Esc route to
  `raise_interrupt`. `deliver_interrupt` re-creates the SIGINT the kernel would
  have sent a foreground *external* child via `interrupt_foreground_child`
  (Windows re-injects `CTRL_C_EVENT`). **Minting a fresh token is the reset** —
  there is no clear-at-every-`apply`, so a just-pressed interrupt is never erased
  before a sub-agent observes it.

## Why interactive Ctrl-C cannot force-exit

A deliberate asymmetry worth stating plainly: **the third-signal `_exit` floor is
unreachable from an interactive prompt.** Interactive SIGINT goes to the relay,
which never increments the counter; the TUI's active-turn Ctrl-C goes to
`raise_interrupt`, which writes a flag and a cause but never the counter.
Repeated presses re-write the same cause (`fetch_max`), never escalate. The hard
floor exists for batch scripts (`handler`) and for an async signal exarch forwards
into ral. Interactive cancellation is cooperative by construction; the root-reap
gesture is REPL Ctrl-`\`, not a TUI key.

## See also

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
