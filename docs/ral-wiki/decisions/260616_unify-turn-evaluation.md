---
status: active
---

# Unify the two turn evaluators; lift "a turn" into `ral_core`

**One persistent `Shell` was driven by two near-duplicate top-level
evaluators, and the divergence between them was a latent scope bug.** The fix
is a single `eval_turn(shell, src, frame) -> TurnOutcome` in `ral_core`,
parameterised by a *frame* — the missing type that carries the policy axes (the
IO regime, the foreground cancel scope, the capability ceiling, lifecycle
callbacks) on which the REPL and exarch differ. Each host stopped reimplementing
evaluation and became a frame supplier that renders the neutral outcome its
own way.

## Context

Two functions run the same top-level spine over a session `Shell`:

- the REPL's [`execute_input`](../../../ral/src/repl/exec.rs) (`ral/src/repl/exec.rs`);
- exarch's [`run_shell`](../../../exarch/src/shell_eval.rs) (`exarch/src/shell_eval.rs`).

Both follow the [[internals/a-turn-end-to-end|a-turn-end-to-end]] spine:
`process::clear()` → `compile_and_typecheck(src, shell.session_schemes())` →
classify a parse/type failure into rendered diagnostics → `install_root_context`
→ `eval_top_level(&comp, shell)` → classify the `Settled<Value>` into an
exit/status. They diverge only on policy:

- **IO.** The REPL runs on the session's ambient live streams (`print_result`,
  its stdout the rustyline `Sink::External` printer, stdin the terminal) and
  installs nothing. exarch installs `Sink::Buffer` captures for stdout/stderr,
  a terminal stdin source, a `surface` host sink that decodes structured events
  onto its rail (`value_to_kind`), and caps/renders the buffered bytes for the
  model transcript.
- **Cancellation.** exarch nests a `timeout_scope = shell.local.cancel.child()`,
  `mem::replace`s it into `shell.local.cancel` for the call, and arms a 30 s
  watchdog thread that calls `scope.cancel()`. The REPL never swaps
  `local.cancel`.
- **Capabilities.** exarch wraps the eval in
  `shell.with_capabilities(caps, …)`; the REPL runs at ambient authority.
- **Lifecycle.** The REPL runs `pre-exec` / `chpwd` / `post-exec` hooks and
  threads the Unix `JobTable` (registering an `Escape::Stopped` pgid as a job).
  exarch runs neither.

These are the *only* differences. Everything load-bearing — the seed from
`session_schemes`, the install-on-`Settled` mobile-persistence contract
([[invariants/turn-ends-ready|turn-ends-ready]]), the `Break`/`Escape`
classification — is duplicated verbatim, so a fix to one path silently
diverges from the other.

## Problem: the swap of `local.cancel` collaterally kills background workers

The divergence is not merely cosmetic. [`Shell::spawn_thread`](../../../core/src/types/shell/inherit.rs)
(`core/src/types/shell/inherit.rs`) parents **every** worker under
`self.local.cancel.child()`. Detached workers (`spawn`, `watch`, and `par`,
which the prelude defines over `spawn`) flow through `spawn_thread`, so the
worker's cancel scope is a child of whatever `local.cancel` holds at spawn time.

During an exarch tool call, `local.cancel` is the per-call `timeout_scope`. A
worker spawned inside that call is therefore a child of the per-call watchdog
scope, and the 30 s `scope.cancel()` reaches it through the scope tree — even
though the worker is meant to outlive the turn and be polled/awaited on later
turns. This is the **collateral kill**: a background worker dies because the
foreground turn that launched it timed out.

The REPL never swaps `local.cancel`, so a worker spawned at a REPL prompt hangs
under the durable session scope and persists across prompts — the intended
behaviour. Ctrl-C is not a proof of a turn scope today: Unix interactive SIGINT
is relay-shaped, while batch SIGINT and exarch Esc feed the process-global
counter. The correctness disagreement is that exarch swaps the worker parent and
the REPL does not.

## Decision

Lift one top-level turn into `ral_core`:

```text
eval_turn(shell: &mut Shell, src: &str, frame: TurnFrame) -> TurnOutcome
```

- The *frame* is the **missing type**. Its fields are the types that already
  exist for each policy axis — never booleans or flags:
  - **IO** — an `IoFrame`, a regime sum, not a stream product: `Inherit`
    installs nothing and runs on the session's ambient live streams, while
    `Capture { stdout, stderr, stdin, surface }` redirects the turn's streams
    into a fresh set the host reads back, with an optional `SurfaceSink`
    decoder. The REPL supplies `Inherit` — its stdout is the rustyline
    `Sink::External` printer, not the terminal directly, and the guard never
    touches it; exarch supplies `Capture` with `Sink::Buffer` captures and a
    surface decoder. The `Inherit | Capture` regime and the turn-local bundle
    it installs are developed further in
    [[decisions/260617_turn-local-state|turn-local-state]].
  - **Foreground scope** — a `CancelScope` under which the turn's foreground
    work runs (see the scope rule below).
  - **Capabilities** — a `Capabilities` to push for the turn's dynamic extent
    via [`with_capabilities`](../../../core/src/types/shell/scope.rs). The
    REPL supplies the ambient `Capabilities::root()`; exarch supplies its grant
    profile.
  - **Lifecycle callbacks** — optional pre/post hooks. The REPL supplies the
    `pre-exec` / `chpwd` / `post-exec` plugin hooks and `Escape::Stopped` job
    registration; exarch supplies none.
- `TurnOutcome` covers the whole top-level turn, not just the runtime result:
  - `Static { diagnostics, status }` for parse/type failures (`status = 1`). No
    root context is installed, no lifecycle hooks run, and the host renders the
    diagnostics.
  - `Runtime { result, eval_status, single_command }` for a compiled turn.
    `result` carries the `Settled<Value>`, whose `Ok` / `Break::Error` /
    `Break::Escape` discriminant already classifies the outcome, so the host
    reads control flow off it directly. The one datum the host cannot otherwise
    recover is `single_command: bool` (from `ir::is_single_command(&comp)`),
    needed to render a runtime error with `format_runtime_error_auto` since
    `comp` is consumed inside `eval_turn`. `eval_status` is computed once by the
    evaluator: `Ok` reads `shell.mobile.control.last_status`, runtime errors
    use their exit code, `exit N` uses `N`, and a stopped job uses
    `128 + signal`.
  The host renders the neutral outcome — `print_result` for the REPL, the capped
  `ToolResult` for exarch — and may map it to a transport status, but it does
  not re-derive the parse/type/runtime classification.
- The save/restore of IO and scope on the persistent `Shell` is RAII, a
  `TurnGuard` ([`core/src/turn.rs`](../../../core/src/turn.rs)) that restores
  the root source context and foreground scope on `Drop` — and, only under a
  `Capture` regime, the prior IO streams and surface sink. exarch continues a
  session on the same `Shell` after a caught worker panic (`bus::pump`'s
  `catch_unwind`), so the teardown must survive an unwind; a hand-written
  restore epilogue would be skipped.

The REPL and exarch become frame suppliers over one evaluator. The duplication —
and the silent divergence — is gone.

## The scope rule: a durable root distinct from the foreground

Record the cancellation/scope structure **once**, as the shape `eval_turn` and
`spawn_thread` both target:

- A **detached worker** (`spawn`, `&` — which the elaborator desugars to
  `spawn` (`core/src/elaborator.rs:578`) — REPL `watch`, and `par`'s
  fan-out over `spawn`) hangs under the shell's **durable root scope**: a
  `CancelScope` on `Shell` that no turn swaps. It is meant to outlive the turn.
- **Pipeline stages** are foreground work. They run under the frame's foreground
  scope directly (`shell.local.cancel`, threaded into the stage at
  `pipeline/launch.rs`), so cancelling the foreground reaches the staged process
  group through that scope's cause. The code carries the group-wide kill and the
  wait→drain typestate; it does **not** mint a separate pipeline-owned
  `local.cancel.child()` — that finer scope edge was left unbuilt, the stages
  share the foreground scope as their cancel handle.
- `par` is *not* a foreground stage: it spawns root-parented workers and
  `await`s them within its own expression, so a cancelled `par` orphans its
  fan-out until explicit `cancel`, a frame lifetime ceiling, root abort, or
  session exit — the accepted cost of keeping `par` sugar over `spawn` rather
  than a structured primitive.
- The frame's foreground scope is always a child of the durable root. The REPL
  leaves it unarmed unless Ctrl-C translation cancels it; exarch arms a watchdog
  or deadline on it. The durable root itself is never installed as
  `shell.local.cancel` for a turn.

This is realised by a **durable root scope on `Shell` distinct from the swappable
`local.cancel`** (`shell.local.durable_root`, of which the turn's foreground
scope is always a child), and by `spawn_thread` parenting its workers at the root
(`durable_root.child()`, `core/src/types/shell/inherit.rs`) rather than at
whatever `local.cancel` holds. The consequence is structural: the 30 s wall
bounds only the foreground subtree and can never reach a background worker,
because the worker is not in that subtree. **The collateral kill is gone by
construction**, not by a guard that remembers to exclude background workers.

## Cancellation: one mechanism, observed through the scope tree

Two cancellation channels exist, with non-uniform routing:

- **The process-global counter** is `SIGNAL_COUNT`
  (`core/src/process/signal.rs`). Batch Unix/Windows signal handlers drive it
  (`fetch_add`), and `process::interrupt` offers a non-escalating `store(1)`.
  `process::check` reads this counter regardless of which scope the polling code
  sits in, so this path reaches the foreground turn *and* every worker that
  polls it. exarch's Esc path does not tick it — that translation reaches the
  foreground scope through a cancel slot (below).
- **Interactive Unix REPL SIGINT** does not use that path. `jobs::setup_signals`
  installs the pipeline relay handler: idle Ctrl-C is a no-op in the shell, and
  active mixed pipelines receive SIGINT by process-group relay. It is not a
  per-turn cancellation mechanism.
- **The scope channel** is a `CancelScope::cancel(cause)`: the per-call deadline
  watchdog, an explicit handle cancellation, and the foreground/root cancel
  slots all act through it. This channel is **scoped**: it reaches exactly the
  subtree of the scope tree it is applied to.

[`process::check`](../../../core/src/process/signal.rs) folds the global counter
and the active scope flag into the same `Break::Error` status (`130`), so callers
need not distinguish them — the global counter reads `"interrupted"`, and a
cancelled scope derives its message from the strongest cause in force
(`"interrupted"` for `Interrupt`, `"cancelled"` for `Explicit`, `"timed out"`
for `Deadline`, `"aborted"` for `RootAbort`).

A scope cancel is not a bare bit. It is a cancelled scope plus a small cause:

```text
enum CancelCause { Interrupt, Explicit, Deadline, RootAbort }
CancelScope::cancel(cause)
```

The cause is semantic, not a transport. `Interrupt` means the user asked the
foreground to stop; `Explicit` means `cancel <handle>` or `race` deliberately
reaped a particular worker; `Deadline` means a wall clock or lifetime ceiling
expired; `RootAbort` means the session root is being reaped. Causes form an
escalation order, `Interrupt < Explicit < Deadline < RootAbort`: repeated Esc /
Ctrl-C is idempotent, explicit handle cancellation can still turn an ignored
interrupt into teardown, a later deadline can supersede both, and Ctrl-`\` can
still supersede everything.

Pollers observe the same scope tree but act by cause:

- The trampoline calls `process::check` at each bounce, so in-process work
  unwinds itself within one reduction step
  ([[decisions/260504_hot-path-cancellation|hot-path-cancellation]]). The
  resulting `Break::Error` message comes from the cause (`"interrupted"`,
  `"cancelled"`, `"timed out"`, or `"aborted"`), not from a parallel global
  counter.
- An external child is awaited by a cancel-aware poll loop
  (`RunningChild::wait`, `core/src/runtime/command/child.rs`), whose
  `terminate_group` branches on the cause. On `Interrupt`, it delivers SIGINT
  to the process group first to preserve job-control semantics, polls a short
  grace, then SIGKILLs the group unconditionally. On `Explicit` or `Deadline`,
  it delivers SIGTERM first under the same grace-then-group-SIGKILL path. On
  `RootAbort`, it sends an immediate group SIGKILL with no foreground grace.

**Ctrl-C, Esc, deadlines, and handle cancellation collapse onto the scope tree,
not onto one child-kill policy.** The triggers differ, the cause records why
they fired, and the poller chooses the local effect. The translation from a
signal or a frontend keypress to a `cancel(cause)` is hosted in `ral_core`:
process-global signal-safe slots `FOREGROUND_SCOPE` and `DURABLE_ROOT_SCOPE`
(`core/src/process/signal.rs`) hold a borrowed pointer into each scope's flag,
published per-turn by `eval_turn` via the RAII `publish_foreground` /
`publish_durable_root` (swap-restore, so nested turns stack). A handler with no
`CancelScope` in hand calls `request_foreground_cancel(cause)` or
`request_root_cancel(cause)` — a lock-free slot load and an atomic `fetch_max`,
itself async-signal-safe:

- **REPL Ctrl-C** during input aborts the line. During foreground eval it
  reaches the published foreground slot with `Interrupt`. Foreground external
  process groups still receive real SIGINT through the terminal / relay
  machinery; detached `spawn` and REPL `watch` workers are spared because they
  hang under the durable root.
- **exarch active-turn Ctrl-C / Esc** cancels the exarch root `Token` and calls
  `request_foreground_cancel(Interrupt)` (`exarch/src/cancel.rs`,
  `deliver_interrupt`). The token stops provider streaming, staged tools, and
  sub-agent turns; the foreground cancel stops the current shell eval. The
  raw-mode path does not tick the global `SIGNAL_COUNT`; repeated presses are
  non-escalating because they write the same cause, not an incrementing counter.
- **Explicit handle cancellation** — `cancel <handle>` and `race` loser
  cancellation — calls the worker scope with `Explicit`, so it uses decisive
  worker teardown without pretending to be a user interrupt or a timeout.
- **Deadlines** — exarch's foreground wall — call `cancel(Deadline)` from the
  per-call watchdog thread (`exarch/src/shell_eval.rs`), keeping the decisive
  SIGTERM→grace→group-SIGKILL behaviour for external child groups.
- **Ctrl-`\` / kill-all** calls `request_root_cancel(RootAbort)`, reaching the
  foreground and every detached worker. The REPL routes it through a SIGQUIT
  handler (`sigquit_handler` in `core/src/process/signal/unix.rs`, installed by
  `jobs::setup_signals`). exarch's TUI does not bind a kill-all key: its raw-mode
  table is idle quit, overlay close, or active-turn interrupt.

Signal-safety is preserved. A Unix signal handler still does only
async-signal-safe atomic work; the translation slot turns the pending event into
`cancel(cause)` on the current foreground or root scope. The semantic collapse is
onto the scope tree and its cause, not into the signal handler itself.

### The trigger: deadlines as data, one reaper — since built

The timeout is realised as a watchdog *thread* per call that sleeps and fires
`scope.cancel(Deadline)`. Firing the flag is that thread's *only* job — the
cause-specific killing lives in the child-wait loop above — so deadlines *want*
to be **data, not threads**: `(scope, when, cause = Deadline)` entries in one
shared timer/reaper service. One service would subsume every deadline — the
foreground wall and the exarch per-worker lifetime ceiling
([[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]])
as entries differing only in scope and `when`, not a thread each; and
thread-per-timeout does not survive the ceiling, since it would mean a thread per
`spawn`. **That service has since been built** in the concurrency-detachment
ADR: `process::reaper` holds `(scope, when)` entries and fires `cancel(Deadline)`
at each. exarch's per-call watchdog thread is gone — `run_shell` arms the
foreground wall as a *disarmable* reaper entry — and the detached-worker
death-clock arms the same service as a *kept* entry. The signal/frontend
translation stays in the `ral_core` cancel slots above; the reaper carries only
the deadline trigger.

### Consequence to flag loudly

Under this design, **Ctrl-C in the REPL and Esc in exarch are foreground
`Interrupt` cancels.** They do not cancel root-parented `spawn` workers or REPL
`watch` workers.
That is a behavioural change for the `SIGNAL_COUNT`-driven paths, and a semantic
clarification for Unix interactive REPL SIGINT, which was previously
relay-shaped rather than turn-shaped. The "panic, kill everything" gesture is a
separate, louder act: **Ctrl-`\` (SIGQUIT) cancels the durable root with
`RootAbort`**, reaping the foreground turn and every detached worker at once.

To keep the new semantics legible, the durable root *would* benefit from a
**worker registry**: `spawn` and REPL `watch` registering their handle
id, display name, cancel scope, and deadline, with a Ctrl-C or Esc that leaves
live registered workers printing a warning naming the survivors and pointing at
Ctrl-`\`. **The registry and survivor warning were not built** — they are
deferred. Without it the host can at best print a count. Ctrl-C/Esc still bound
the foreground; Ctrl-`\` still bounds the session.

## Consequences

- The mobile-persistence contract, the seed from `session_schemes`, and the
  `Break`/`Escape` classification live in one place; a fix lands once.
- A worker reaped by a foreground `Interrupt` or `Deadline` can no longer happen
  by accident; it requires cancelling the root or the worker's own registered
  lifetime deadline.
- **A per-worker lifetime bound was out of scope here, and has since been
  built.** The scope rule keeps a foreground timeout from reaching a background
  worker, but it says nothing about a non-terminating background worker an agent
  abandons. In a long-running exarch session those accumulate. The complementary
  *death-clock* — a per-worker lifetime cap that reaps an abandoned worker — was
  built in
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]];
  this ADR fixed the collateral kill, that one the reaping.

### What shipped, what was deferred

The decision landed across nine commits (`0b561ed5` … `5ff2bab1`). **Shipped:**
`eval_turn` and `TurnFrame` in `core/src/turn.rs`, with both hosts as frame
suppliers (the REPL's `execute_input`, exarch's `run_shell`); the `IoFrame`
regime sum (`Inherit` | `Capture`); the durable-root scope on `Shell`
(`durable_root`, of which `local.cancel` is always a child) and `spawn`
parenting workers at the root, fixing the collateral kill by construction; the
`CancelCause` escalation order and the cause-aware `terminate_group`; the
`ral_core` cancel-translation slots and their per-turn publication; and the
re-routed Ctrl-C / Esc (`Interrupt`) and Ctrl-`\` (`RootAbort`) triggers.

**Deferred or not adopted:**

- The **worker registry and survivor warning** were not built.
- The **single timer/reaper service** ("deadlines as data") was deferred here
  but **has since been built** (`process::reaper`) in
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]];
  exarch's per-call watchdog thread is gone, the foreground wall now a
  disarmable reaper entry.
- The **death-clock** / per-worker lifetime cap was out of scope here but **has
  since been built** in the same ADR: exarch arms a one-hour ceiling on each
  detached `spawn` worker via the reaper, so a worker abandoned across turns is
  reaped by its ceiling rather than only by Ctrl-`\` during a turn.
- The **`ral` batch path** (`ral/src/main.rs`, calling `eval_top_level`
  directly) was **not** unified through `eval_turn` — a known remaining gap.

### The drain↔cancel ordering risk

The teardown order for a captured turn is load-bearing and must be an explicit
frame step. exarch must, in order: **cancel → drain the stdout pump → collect
the bytes.** If bytes are collected before the pump that fills them has
drained, the transcript loses the child's tail output.

Today this ordering holds, but it is realised in two places rather than named
once:

- *Inside the evaluator,* per external child, it factors cleanly.
  [`RunningChild::observe`](../../../core/src/runtime/command/child.rs)
  (`core/src/runtime/command/child.rs`) is `wait()` → classify → `drain()`,
  and a typestate enforces the order: `drain` is only invocable on a
  `WaitedChild`, which only `wait` can construct, so "join the pump before the
  child is dead" is unwritable. On the cancel branch, `wait`'s poll loop reads
  `self.cancel.cause()` and runs `terminate_group` with that cause — the
  `Deadline` branch SIGTERMs the process *group*, polls a short grace, then
  SIGKILLs the group; the `Interrupt` branch sends SIGINT first to preserve
  job-control semantics under the same grace-then-group-SIGKILL path — and only
  then falls through to the blocking reap and `drain()`. Killing the whole group
  — not just the leader pid — closes the pipe a forked grandchild would otherwise
  hold open, so the pump's `drain()` join completes instead of blocking. This is
  exactly what
  `shell_eval::tests::timeout_kills_external_subprocess_tree` and the core
  `child::tests::interrupt_tears_down_external_subprocess_tree` pin: a
  `/bin/sh -c 'sleep 30 & echo $!; wait'` returns at the deadline with a reaped
  grandchild.

- *In exarch's host frame,* the ordering is **implicit, not factored.**
  `run_shell` collects bytes (`std::mem::take` on the `stdout_buf`/`stderr_buf`
  arcs) only after `eval_turn(shell, cmd, frame)` returns, and `eval_turn`
  returns only once the in-process eval has finished tearing down its children —
  so the collection *happens* to follow the drain. But nothing in the frame
  names "drain the pumps, then collect"; it rides on the synchronous structure
  of `run_shell`.

**Finding:** the per-child drain factors cleanly (typestate-enforced inside
`observe`); the host-level cancel→drain→collect ordering does **not** factor —
it stays an emergent property of `run_shell`'s straight-line structure. Byte
collection was **not** made an explicit post-drain frame step, so the
abstraction still leaks here: an `eval_turn` that returned before the foreground
subtree's pumps had drained (e.g. a turn that backgrounds its tail) would hand
the host a half-filled buffer with no compiler complaint. A named frame teardown
phase — cancel the foreground scope, join the foreground pumps, then read the
buffers — would close the leak, and remains future work.

## Alternatives considered

- **Keep two paths.** Rejected: the collateral-kill bug is precisely the cost
  of two paths that disagree on whether to swap `local.cancel`. Every shared-
  spine fix risks re-diverging.
- **One function with boolean flags** (`eval_turn(shell, src, buffered: bool,
  timeout: Option<Duration>, caps: …)`). Rejected: it reintroduces the
  flag-soup the *frame as a type* exists to avoid, and it does not carry the
  IO sinks or the foreground scope — the axes that actually differ. The frame's
  fields are the existing semantic types (`Sink`, `CancelScope`,
  `Capabilities`), per [[design/grant|grant]] and the host-embedding seam
  ([[decisions/260610_host-embedding-api|host-embedding-api]]).
- **Turn-as-nursery** — make every worker a child of the turn's scope, joined at
  the turn boundary. Rejected: `spawn`'s entire purpose is cross-turn
  persistence (the model launches work, the turn returns, later turns
  `poll`/`await` the handle — the
  [`ral`](../../../exarch/src/tools/ral.rs) tool's timeout message says exactly
  this). A nursery would kill the worker at the turn boundary, which is the
  collateral kill generalised.
- **Detached-by-default for *all* workers, no foreground scope at all.**
  Rejected: pipeline stages *should* die with their command — they belong to the
  command, not the session — so a foreground scope is needed for them. `spawn`,
  `&`, REPL-only `watch`, and the `par` fan-out built over `spawn` are the
  detached class that outlives the turn. The root-vs-foreground split is the
  point, not a uniform detachment.

## Open questions

### Resolved

- **The death-clock** — *resolved, built in
  [[decisions/260616_concurrency-primitives-detached-vs-structured|concurrency-detached-vs-structured]].*
  The per-worker lifetime cap that reaps abandoned non-terminating background
  workers: exarch arms a one-hour ceiling on each detached `spawn` worker via the
  shared `process::reaper`, so a worker abandoned across turns is reaped by its
  ceiling. The default cap and its frame-ownership live in that ADR.
- **Translation-service placement** — *resolved, in `ral_core`.* The signal /
  frontend translation routes events into `CancelScope::cancel(cause)` through
  process-global signal-safe slots `FOREGROUND_SCOPE` / `DURABLE_ROOT_SCOPE`
  (`core/src/process/signal.rs`), published per-turn by `eval_turn` and reached
  by `request_foreground_cancel` / `request_root_cancel`. The slots live in core,
  separate from the deadline trigger, which now rides the shared `process::reaper`
  (the per-call watchdog thread is gone). exarch's chained handler and per-turn `Token`
  ([[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]) sit alongside:
  the `Token` halts the turn loop and provider streaming, the foreground slot
  halts the in-flight ral eval.

## Consistency note

The exarch [`ral`](../../../exarch/src/tools/ral.rs) tool fixes its inline
timeout at 30 s with no per-call knob (`CALL_TIMEOUT_SECS`,
`exarch/src/tools/ral.rs`), and its timeout message and the prompt steer long
work toward `spawn`/`poll`/`await`. This design preserves that: the watchdog
is a property of exarch's frame, not a tool-level parameter, and nothing here
re-adds a per-call timeout argument.

See also [[internals/a-turn-end-to-end|a-turn-end-to-end]] (the shared spine
this lifts), [[decisions/260504_hot-path-cancellation|hot-path-cancellation]]
(where the evaluator polls the scope), [[decisions/260612_per-root-turn-cancel|per-root-turn-cancel]]
and [[decisions/260608_esc-non-escalating-interrupt|esc-non-escalating-interrupt]]
(exarch's Esc/SIGINT half), [[decisions/260610_host-embedding-api|host-embedding-api]]
(the embedding seam this extends), [[map/repl/loop|repl/loop]],
[[map/exarch/shell-eval|exarch/shell-eval]], and
[[invariants/turn-ends-ready|turn-ends-ready]].
