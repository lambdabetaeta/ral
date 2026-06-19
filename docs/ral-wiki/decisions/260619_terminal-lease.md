---
status: proposed
---

# A held terminal lease, not an inferred predicate, gates the foreground handoff

**The authority to hand the controlling terminal to a child should be an
unforgeable value a turn is *given* — a `TerminalLease` — not a predicate every
launch path re-derives from process-global startup state.** Today three
mechanisms encode that authority ambiently — `startup_foreground` (the
capability), `JobControl` (orchestrator-vs-stage permission), and the
`capture_depth`/`tui_active` pair (per-pipeline permission plus dynamic
suppression) — and two launch paths (standalone, pipeline) re-infer the decision
from them independently. The lease collapses all of that into one question asked
at the one place a `tcsetpgrp` can happen: *do you hold the lease?* Code that was
not handed it cannot construct the handoff, so an exarch tool turn that
foregrounds a child — the SIGTTIN crash this ADR is written against — becomes a
state the type system refuses to represent. This supersedes
[[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]],
whose `startup_foreground` predicate the lease reifies as a value.

This is a proposal; nothing has landed. Today the handoff is gated by reading
`shell.turn.io.terminal.startup_foreground` at two decision sites and authorised
again inside `ForegroundGuard::try_acquire` (`core/src/process/signal/unix.rs:534`);
`resolve_terminal_plan` (`core/src/runtime/pipeline/resolve.rs:302`) bolts on a
`capture_depth > 0 && !tui_active` exception; exarch tool turns inherit
`startup_foreground = true` and so foreground their pipelines.

## The diagnosis

### The live bug

An exarch tool turn runs `run_turn` under `TurnIo::Capture`
(`exarch/src/shell_eval.rs:97`). A bare top-level pipeline in that turn —
`git diff | from-string` — reaches `resolve_terminal_plan`, which:

1. passes the `startup_foreground` gate (`resolve.rs:302`): exarch boots an
   interactive TUI in the foreground, so its `probe_foreground` returns true
   (`core/src/io/terminal.rs:312`);
2. skips the capture short-circuit (`resolve.rs:320`): `build_turn`
   (`core/src/turn.rs:143`) swaps the three Capture streams but never raises
   `capture_depth`, which only the `!{…}` audit guard touches
   (`core/src/evaluator/capture.rs:43`), so it is `0`;
3. returns `ForegroundExternalGroup`.

The pipeline then `tcsetpgrp`s the child pgid (`launch.rs:322` →
`PipelineGroup::claim_foreground` → `ForegroundGuard::try_acquire`), dropping the
whole exarch process to a background process group. The TUI input thread's
`ct_read()` (`exarch/src/tui.rs:2466`, `:2501`) then reads the controlling
terminal from the background and the kernel raises **SIGTTIN**, stopping the
process. The same root has a second head: the Capture path mints
`Source::Terminal` as stdin (`core/src/host.rs:270`), so a tool command that
reads stdin steals input from the TUI even with SIGTTIN fixed. Both are one
fact — *an exarch tool turn must not touch the controlling terminal at all.*

### Why a point-fix recurs

The decision "may this launch foreground a child?" is answered by reading
process-global and turn-local state at **two** sites that must agree:

- **standalone** — `ForegroundDecision::for_standalone`
  (`core/src/runtime/command/foreground.rs:54`) reads
  `job_control.may_foreground()` ∧ `startup_foreground` ∧ stdout-sink-shape
  (`Sink::Terminal | Sink::External`) ∧ no-pump;
- **pipeline** — `resolve_terminal_plan` (`resolve.rs:295`) reads
  `startup_foreground` ∧ `capture_depth` ∧ `tui_active` ∧ `!windows`.

These are two inferences over the same underlying fact, and they have drifted
apart before: the regression tests in `resolve.rs` record three separate
SIGTTOU/SIGTTIN incidents already — the `run-claude.ral` teardown
(`non_interactive_terminal_script_foregrounds_pipeline`), the CTRL-R/fzf history
failure (`ed_tui_capture_still_foregrounds_pipeline`), and now this. Each was
closed by adding or tuning a gate. The defect is not a missing gate; it is that
the authority is *inferred* from ambient state rather than *held*. Adding a
fourth condition (raise `capture_depth` in `build_turn`) would be the duct-tape
continuation of exactly this pattern, and it overloads a counter documented as
"*depth of nested `with_capture` scopes*" (`core/src/io.rs:99`) to mean
something it does not.

### The one chokepoint already exists

Every `tcsetpgrp` in the workspace funnels through a single RAII type,
`ForegroundGuard::try_acquire(target, shell)` (`signal/unix.rs:533`), from all
three callers — standalone (`foreground.rs:132`), pipeline launch
(`launch.rs:322`), and `fg`-resume (`ral/src/jobs.rs:364`). The guard already
snapshots and restores pgid + termios on `Drop` (`signal/unix.rs:613`) and
already re-checks `startup_foreground` on acquire. There is exactly one door to
gate; the lease is the key the door demands.

## The decision

### 1 — The token: an unforgeable, core-minted, linear value

```rust
// core::process — constructor private to this module
pub struct TerminalLease { _seal: () }

impl TerminalLease {
    /// The one mint. Succeeds iff ral owns the controlling terminal's
    /// foreground at process entry (tcgetpgrp(0) == getpgrp()); None on
    /// Windows, a non-tty stdin, or a backgrounded launch. This is the body
    /// of `probe_foreground` (io/terminal.rs:312), now producing a value
    /// instead of a bool.
    pub fn mint() -> Option<Self>;
}
```

`TerminalLease` is **not `Clone`, not `Copy`**, and constructible only inside
`core::process`. Its *existence* is the capability — the host holds at most one,
and only if ral genuinely owned the tty at boot. The `startup_foreground` bool
(`io/terminal.rs:95`) is then deletable as authority: nothing reads it but the
foreground decision (it is not in the `$TERMINAL` map exposed to scripts), so
"a lease exists" carries its whole meaning. This is the witness discipline of
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]] applied
to the terminal: a capability that was a readable flag becomes a value you must
be handed.

### 2 — The chokepoint demands the lease

```rust
// was: try_acquire(target, shell) — read shell.…startup_foreground
pub fn try_acquire(target: pid_t, _lease: &TerminalLease) -> Option<ForegroundGuard>;
```

With this one signature change it is *uncompilable* to hand off the terminal
without a `&TerminalLease` in scope. Because there is one chokepoint, the
type-level invariant the diagnosis wants — "code without a lease cannot steal the
terminal" — costs one parameter, not a sweep. The guard's internal
`startup_foreground` re-check goes: holding a `&TerminalLease` *is* that proof.

### 3 — A turn is granted access, not handed a pre-computed plan

`TurnRequest` ([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]])
gains a field that states the host's intent for the turn:

```rust
pub enum TerminalAccess {
    /// This turn may foreground children: its launchers can reach the lease.
    Leased,
    /// This turn never touches the controlling terminal: no lease, and a
    /// null stdin source.
    Denied,
}
```

- **interactive REPL turn** → `Leased`.
- **terminal-launched script** (`ral run-claude.ral`) → `Leased` (the second
  regime of
  [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]],
  preserved).
- **exarch tool turn** → `Denied`. No lease reaches the launcher, so no
  tool-turn pipeline can `tcsetpgrp` — **the SIGTTIN bug is unrepresentable** —
  and `Denied` also routes a null stdin instead of `Source::Terminal`
  (`host.rs:270`), closing the input-stealing half.
- **piped `ral -c`, backgrounded `ral … &`** → `Denied` (and `mint` would
  return `None` for them anyway).

### 4 — `_ed-tui` is an explicit loan, and `tui_active` dies

The `_ed-tui` case (an editor body like `fzf` that draws on `/dev/tty` while its
stdout is captured to read the selection) is today the *negative exception*
bolted onto the capture gate — `capture_depth > 0 && !tui_active`
(`resolve.rs:320`) — with `tui_active` threaded across pipeline frames by the
REPL scratch (`core/src/types/shell/repl.rs:59`, `:85`, `:96`) and set/cleared by
the editor builtin (`ral/src/repl/plugin_ed_builtins.rs:238`). Under the lease it
becomes a *positive* borrow:

```rust
let _loan = lease.loan();   // host suspends its own TUI; body draws on /dev/tty
//   run the _ed-tui body with TerminalAccess::Leased
// _loan drop → reclaim tty, restore termios + pgid, resume host TUI
```

`tui_active`, its STT-in/out plumbing in `repl.rs`, and the `resolve.rs`
exception all delete. The reason the current code cannot simply read the stdout
sink shape — `_ed-tui` has a buffer sink yet *wants* foreground — is exactly what
the loan states directly: this turn's body owns the tty regardless of where its
bytes go.

### 5 — What collapses into the lease

| Ambient mechanism today | Becomes |
| --- | --- |
| `startup_foreground` bool (`terminal.rs:95`) | the lease's *existence* (`mint` is `probe_foreground`) |
| `JobControl{top_level,pipeline_child}` (`io.rs:42`) | *who holds the `&lease`* — the pipeline launcher does, a stage never does |
| `capture_depth`'s foreground gate (`resolve.rs:320`) | lease present ∧ final sink terminal-bound (the standalone path already reads the sink, `foreground.rs:57`) |
| `tui_active` (`repl.rs:59`) | the explicit `lease.loan()` |

`JobControl`'s job — "only the pipeline launcher may hand off the terminal"
(`io.rs:38`) — is enforced by *ownership*: the launcher is handed the `&lease`,
stages are not, and linearity keeps a stage from forging one. `capture_depth`
keeps its unrelated `Seq`-flush role (`io.rs:99`, `capture.rs`); only its
consultation in `resolve_terminal_plan` is removed. The post-lease rule at the
pipeline door is a single line:

> foreground iff the turn is `Leased` **and** (the pipeline's final stdout is
> terminal-bound **or** the access is an explicit tty loan).

## Why this shape

- **The authority is held, not inferred.** One value answers "may I foreground?"
  at the one door, so the standalone and pipeline paths cannot drift apart — the
  failure mode behind all three recorded regressions.
- **It removes code.** The lease is not a fourth mechanism beside the three; it
  *is* the three, unified, with `startup_foreground` (as authority), `JobControl`,
  and `tui_active` deleted. Net subtraction, in the spirit of
  [[decisions/260614_structural-bug-prevention|structural-bug-prevention]]:
  make the bad state unconstructable with a type, do not guard it at dispatch.
- **The chokepoint is already there.** One RAII type, one `try_acquire`, three
  callers. The invariant is a one-parameter change, not a refactor of the
  foreground machinery.
- **It fits the turn-frame model.** Terminal access is turn-local state, exactly
  like the foreground `CancelScope` and `SurfaceSink` that already ride
  `TurnState` and are restored by `TurnGuard` on teardown
  ([[decisions/260617_turn-local-state|turn-local-state]],
  `core/src/turn.rs:96`).
- **Windows is untouched.** `mint` returns `None` (no `tcsetpgrp`), so every turn
  is effectively `Denied`; the helper protocol is unchanged, as in
  [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]].

## The one open decision: pure-linear vs. parked token

The lease's linearity meets one real obstacle — the evaluator threads
`&mut Shell` through a recursive trampoline, and the `Shell` outlives any single
turn, so a `&'a TerminalLease` cannot be stored on it. Two ways to carry the
lease to the deep launcher, and this is the decision left to the author:

- **Recommended — parked token + per-turn access.** The single owned
  `TerminalLease` lives on `session`; `TurnState` carries the `TerminalAccess`
  marker, restored by `TurnGuard` like `cancel`/`surface` already are. The
  launcher obtains `&lease` only when its turn is `Leased`. This delivers the two
  guarantees that matter — *type-level at the door* (`try_acquire` needs
  `&TerminalLease`, unforgeable and core-private) and *structural for exarch*
  (`Denied` produces no `&lease` for the launcher) — without touching evaluator
  threading. It is lease-flavoured, not purely linear: the token is reachable via
  `&session`, but it cannot be forged, cloned, or minted outside its capability.

- **Alternative — pure linear.** Move the lease (and loans) by value through the
  turn and the pipeline build, so the borrow checker enforces single-ownership
  end to end with no parked token. This is the strongest invariant, but it fights
  the `&mut Shell` recursion at every frame the evaluator descends and is a large,
  invasive change for the increment over the parked form. Recommended only if a
  future feature needs to *move* terminal ownership across components rather than
  lend it for a turn.

Everything else in this ADR is independent of the fork; the door, the
`TerminalAccess` field, and the deletions are identical either way.

## Alternatives considered

- **Raise `capture_depth` in `build_turn` for Capture turns.** Rejected: a
  one-line symptom fix that overloads a counter meaning "`!{…}` nesting depth"
  (`io.rs:99`) to also mean "this turn's IO is captured," adds a fourth
  interacting condition to the heuristic that already produced three regressions,
  and does not touch the stdin half.
- **Explicit per-turn foreground policy, but keep `startup_foreground` /
  `JobControl` as-is.** This is a way-station, not a rejection — it *is* steps
  1–2 of the plan below stopping early. It fixes the live bug, but leaves the two
  inference sites and the `tui_active` exception standing, so the
  drift-between-paths failure mode survives. The lease is this idea carried to
  the point where the ambient inputs are deleted.
- **Defensive `tcgetpgrp == getpgrp()` guard before `ct_read` in exarch.**
  Legitimate belt-and-suspenders hardening (the TUI input loop arguably should be
  SIGTTIN-robust regardless), but it papers over the symptom — it does not stop
  the wrong handoff. Keep as optional hardening *in addition to*, never instead
  of, the lease.

## What changes, what stays

- **New:** `TerminalLease` (`core::process`), `TerminalAccess` on `TurnRequest`,
  `lease.loan()` for the `_ed-tui` borrow.
- **Deleted:** `startup_foreground` as a field/authority (folded into `mint`),
  `JobControl` (`io.rs:42`), `tui_active` and its `repl.rs` STT plumbing, the
  `resolve.rs:320` capture exception.
- **Narrowed:** `resolve_terminal_plan` to the one-line lease rule;
  `try_acquire` to take `&TerminalLease`; `ForegroundDecision` to consult the
  lease instead of `job_control` + `startup_foreground`.
- **Unchanged:** `ForegroundGuard`'s pgid/termios save-restore and SIGTTOU mask
  (`signal/unix.rs:613`), the pipeline group/anchor/relay machinery, parking-on-stop
  staying REPL-only, `capture_depth`'s `Seq`-flush role, and the helper protocol
  on every platform.

## Consequences

- The terminal-foreground decision has one source of truth (the lease at the
  door), so the standalone and pipeline paths cannot disagree about which shells
  own the terminal — the class behind all three recorded regressions closes.
- An exarch tool turn that foregrounds a child, or reads the TUI's stdin, is no
  longer a bug to guard against; it does not typecheck.
- Three ambient inputs disappear; `resolve_terminal_plan` shrinks to one rule.
- The fork (parked vs pure-linear) trades a small residual of reachable-but-
  unforgeable state for not rewriting evaluator threading.
- [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]
  becomes *superseded* when this lands: its predicate is the lease's mint
  condition, its regimes are the `TerminalAccess` values, and its SIGTTOU-mask
  restore is retained verbatim.

## Implementation plan

Documentation of intended work, not a commitment to build now. Five parcels;
each compiles and tests alone. Parcels 1–2 are the high-value core and resolve
the live crash; 3–4 pay down the three-regression debt; 0 picks the fork.

```
0  Fork         decide parked-token vs pure-linear (the open decision above); the rest is identical either way
1  Token+door   introduce TerminalLease; mint() = probe_foreground body; try_acquire takes &TerminalLease;
                thread the lease to the 3 callers (standalone, pipeline finish, fg-resume). Behaviour identical — capability is now type-level.
2  Access       add TerminalAccess to TurnRequest; exarch passes Denied (+ null stdin); REPL/script pass Leased;
                resolve_terminal_plan + ForegroundDecision consult it.  ← fixes the SIGTTIN + stdin bug
3  Loan         convert _ed-tui to lease.loan(); delete tui_active, its repl.rs STT plumbing, and the resolve.rs:320 exception
4  Cleanup      delete JobControl and startup_foreground (as authority); collapse resolve_terminal_plan to the one-line lease rule
```

## Test plan

- **Regression port.** The three `resolve.rs` foreground tests are rewritten in
  lease terms and stay green: an interactive `Leased` turn foregrounds a
  terminal-bound pipeline; a `Denied` (exarch) turn never does; a terminal-launched
  script (`Leased`) still foregrounds an interactive child; a backgrounded shell
  (no lease) does not.
- **The bug, pinned.** An exarch tool turn running `git diff | from-string` issues
  no `tcsetpgrp` (a `Denied` turn cannot reach `try_acquire`); a tool command that
  reads stdin sees an empty source, not the controlling terminal — the
  `shell_eval.rs` harness asserts both.
- **The capture cases.** Inside a `Leased` turn, `!{ git diff | grep x }` does not
  foreground (buffer sink, no loan); a top-level `claude` does (terminal sink);
  `_ed-tui` running `fzf` does (explicit loan, despite its captured stdout) — the
  CTRL-R regression, re-pinned without `tui_active`.
- **Type-level door.** A compile-fail test (or a `// must not compile` note) that
  `ForegroundGuard::try_acquire` cannot be called without a `&TerminalLease`, and
  that `TerminalLease` has no public constructor outside `core::process`.
- **Restore unchanged.** The pgid/termios round-trip and SIGTTOU mask behave as
  the [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]
  tests already assert; the lease changes who may acquire, not how release works.

## See also

[[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]
(superseded — its predicate is the lease's mint condition),
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]] (the
witness discipline: a capability becomes a value you must be handed),
[[decisions/260614_structural-bug-prevention|structural-bug-prevention]] (make
the bad state unconstructable with a type; lint as backstop),
[[decisions/260617_turn-local-state|turn-local-state]] (terminal access rides
`TurnState`, restored on teardown),
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]] (`TurnRequest` is
the host-intent seam `TerminalAccess` joins),
[[internals/pipeline-execution|pipeline-execution]],
[[map/core/io-process|io-process]], [[map/core/runtime|runtime]],
[[map/repl/jobs|jobs]].
