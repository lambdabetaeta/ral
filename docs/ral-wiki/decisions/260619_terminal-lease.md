---
status: active
---

# A held terminal lease, not an inferred predicate, gates the foreground handoff

**The authority to hand the controlling terminal to a child should be an
unforgeable value a turn is *given* — a `TerminalLease` — not a predicate every
launch path re-derives from process-global startup state.** Today three ambient
facts stand in for that authority — `startup_foreground` (the capability),
`JobControl`'s foreground role (orchestrator-vs-stage permission), and the
`capture_depth`/`tui_active` pair (per-pipeline permission plus dynamic
suppression) — and two launch paths (standalone, pipeline) re-infer the decision
from them independently. The lease collapses those three into one question asked
at the one place a child/job foreground handoff can happen after startup: *do you
hold the lease?* It also disentangles a fourth concern the same `JobControl` value
carries — the process-group topology role (`top_level` vs pipeline stage), which
the lease leaves alone and a later cleanup narrows to a `LaunchRole`. Code that
was not handed the lease cannot construct the handoff, so an exarch tool turn that
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

### The post-startup chokepoint already exists

Every child/job foreground handoff after REPL startup funnels through the same
RAII type, `ForegroundGuard::try_acquire(target, shell)` (`signal/unix.rs:533`):
standalone (`foreground.rs:132`), pipeline launch (`launch.rs:322`), and
`fg`-resume (`ral/src/jobs.rs:364`). The REPL's initial job-control ceremony is
separate (`ral/src/repl/session/boot.rs:85`) because it first becomes the
foreground shell; the lease gates the later handoffs that a turn may attempt.
The guard already snapshots and restores pgid + termios on `Drop`
(`signal/unix.rs:613`). There is exactly one post-startup door to gate; the lease
is the key the door demands.

## The decision

### 1 — The token: one session-owned witness

```rust
// core::process — fields and constructor private to core
pub struct TerminalLease { _seal: () }

impl TerminalLease {
    pub(crate) fn mint_at_startup() -> Option<Self>;
}

pub(crate) struct SessionState {
    terminal_lease: Option<TerminalLease>,
    // …
}
```

`TerminalLease` is **not `Clone`, not `Copy`**, and has no public constructor or
public `mint`. Core mints at most one token while constructing the session, from
the same `tcgetpgrp(stdin) == getpgrp()` predicate that currently populates
`startup_foreground` (`core/src/io/terminal.rs:312`). Hosts cannot re-run the
predicate later and cannot forge a new proof; they can only ask core to run a
turn with a stated terminal policy. The `startup_foreground` bool
(`io/terminal.rs:95`) may survive the first parcel as compatibility data, but it
is no longer an authority source once every handoff demands `&TerminalLease`.

This is the witness discipline of
[[decisions/260601_reduced-authority-witness|reduced-authority-witness]] applied
to the terminal: a readable flag becomes a capability value that only the runtime
can hold.

### 2 — The chokepoint demands the lease

```rust
// was: try_acquire(target, shell) — read shell.…startup_foreground
pub fn try_acquire(target: pid_t, _lease: &TerminalLease) -> Option<ForegroundGuard>;
```

With this one signature change it is *uncompilable* to hand off the terminal
without a `&TerminalLease` in scope. The guard's internal
`startup_foreground` re-check goes: holding a `&TerminalLease` *is* that proof.
The three post-startup callers receive the borrow only through authorised code.
Core provides a narrow handoff accessor that returns `Some(&TerminalLease)` only
when the installed turn is `Leased` or `ExplicitLoan` and the session owns the
lease. `fg` resume uses that same accessor; exarch tool turns install `Denied`,
so the borrow is unavailable there.

- standalone foreground commands;
- foreground pipeline launch;
- `fg` resume of a parked job.

### 3 — Foreground authority and stdin policy are separate

`TurnRequest` ([[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]])
gains two independent host-facing fields: one says whether a turn may foreground
children, the other says what byte source stdin should use. `TurnState` carries a
richer internal access value because `_ed-tui` can temporarily elevate an already
leased turn.

```rust
pub enum RequestedTerminalAccess {
    /// No child/job foreground handoff in this turn.
    Denied,
    /// This turn may foreground terminal-bound children.
    Leased,
}

pub enum TurnStdin {
    /// Use the session stdin source. Today this is the inherited fd 0, which may
    /// be a terminal, pipe, or redirected file.
    Inherit,
    /// Install an empty source; no fall-through to fd 0.
    Empty,
}

pub(crate) enum TerminalAccess {
    Denied,
    Leased,
    ExplicitLoan,
}
```

- **interactive REPL turn** → `Leased` + `Inherit`.
- **terminal-launched script** (`ral run-claude.ral`) → `Leased` + `Inherit`,
  preserving the second regime of
  [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]].
- **exarch tool turn** → `Denied` + `Empty`. No lease reaches the launcher, so no
  tool-turn pipeline can `tcsetpgrp` — **the SIGTTIN bug is unrepresentable** —
  and no tool command can read the TUI's controlling terminal.
- **piped `ral -c`** → `Denied` + `Inherit`: it may read its pipe while lacking
  foreground authority.
- **backgrounded `ral … &`** → no lease; the host chooses stdin explicitly rather
  than smuggling terminal-read permission through foreground denial.

### 4 — `_ed-tui` is an explicit loan, and `tui_active` dies

The `_ed-tui` case (an editor body like `fzf` that draws on `/dev/tty` while its
stdout is captured to read the selection) is today the *negative exception*
bolted onto the capture gate — `capture_depth > 0 && !tui_active`
(`resolve.rs:320`) — with `tui_active` threaded across pipeline frames by the
REPL scratch (`core/src/types/shell/repl.rs:59`, `:85`, `:96`) and set/cleared by
the editor builtin (`ral/src/repl/plugin_ed_builtins.rs:238`). Under the lease it
becomes a *positive* borrow. `_ed-tui` runs as a builtin *inside* an already
`Leased` REPL turn, so the loan is a within-turn RAII elevation of the installed
turn's access — mirroring exactly how `tui_active` is set and cleared mid-turn
today — not a value a host passes at `run_turn`:

```rust
let _loan = terminal_lease.loan();   // host suspends its own terminal surface;
//                                   // raises the installed turn to ExplicitLoan
// run the editor body (e.g. fzf)
// drop → restore the turn's access, reclaim tty, restore termios + pgid, resume host surface
```

`TerminalAccess::ExplicitLoan` is the only state in which a foreground handoff
fires while stdout is a buffer. The host-side loan guard must suspend any live
terminal reader/renderer before the child owns `/dev/tty`, then restore pgid +
termios and resume the host surface on drop. `tui_active`, its STT-in/out plumbing
in `repl.rs`, and the `resolve.rs` exception all delete.

### 5 — What collapses into the lease

| Ambient mechanism today | Becomes |
| --- | --- |
| `startup_foreground` bool (`terminal.rs:95`) | the session's `Option<TerminalLease>` |
| foreground part of `JobControl{top_level,pipeline_child}` (`io.rs:42`) | a borrow of `&TerminalLease` available only to launch orchestration |
| process-group role part of `JobControl` | retained first, then renamed/narrowed to an explicit `LaunchRole` |
| `capture_depth`'s foreground gate (`resolve.rs:320`) | final-sink policy: terminal-bound stdout, unless `ExplicitLoan` |
| `tui_active` (`repl.rs:59`) | `TerminalAccess::ExplicitLoan` plus a host loan guard |

`JobControl` is not deleted in the first pass: it also encodes process-group
topology (`TopLevel` vs pipeline stage) and keeps pipeline helpers from becoming
new leaders on their own. The lease removes terminal handoff authority from that
type; a later cleanup can rename the remaining role to `LaunchRole`. `capture_depth`
keeps its unrelated `Seq`-flush role (`io.rs:99`, `capture.rs`); only its
consultation in `resolve_terminal_plan` is removed. The post-lease rule at the
pipeline door is:

> foreground iff the turn has a lease **and** (`stdout` is terminal-bound **or**
> the turn is an explicit tty loan).

## Why this shape

- **The authority is held, not inferred.** One value answers "may I foreground?"
  at the one door, so the standalone and pipeline paths cannot drift apart — the
  failure mode behind all three recorded regressions.
- **It removes authority, then code.** The lease is not a fourth gate beside the
  existing gates; it is the value the gates were approximating. The first parcels
  keep process-group role plumbing where it still carries a distinct fact, then
  delete `startup_foreground` as authority and `tui_active` outright.
- **The chokepoint is already there.** One RAII type, one post-startup
  `try_acquire`, three callers. The invariant is a one-parameter change, not a
  refactor of the foreground machinery.
- **It fits the turn-frame model.** Terminal access is turn-local state, exactly
  like the foreground `CancelScope` and `SurfaceSink` that already ride
  `TurnState` and are restored by `TurnGuard` on teardown
  ([[decisions/260617_turn-local-state|turn-local-state]],
  `core/src/turn.rs:96`).
- **Windows is untouched.** The startup mint produces no lease (no `tcsetpgrp`),
  so every foreground handoff is unreachable; the helper protocol is unchanged,
  as in [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]].

## Carrier choice: parked token, not pure-linear threading

The chosen carrier is the **parked token + per-turn access** form. The single
owned `TerminalLease` lives on session state; `TurnState` carries
`TerminalAccess`, restored by `TurnGuard` like `cancel` and `surface` already
are. A launcher obtains `&TerminalLease` only when the installed turn is
authorised and the session actually owns a lease.

This is not fully linear in the type-theory sense: the token is parked in
session state rather than moved down the evaluator spine. It still delivers the
load-bearing guarantees:

- `ForegroundGuard::try_acquire` is uncallable without an unforgeable
  `&TerminalLease`.
- Exarch installs `TerminalAccess::Denied`, so its tool turns cannot obtain that
  borrow.
- Evaluator threading stays unchanged; the recursive trampoline keeps carrying
  `&mut Shell` instead of a moving terminal token.

The pure-linear alternative — move the lease and loans by value through the turn
and pipeline build — is stronger but invasive. It should wait for a feature that
needs to *move* terminal ownership between components rather than lend it for a
turn.

## Alternatives considered

- **Raise `capture_depth` in `build_turn` for Capture turns.** Rejected: a
  one-line symptom fix that overloads a counter meaning "`!{…}` nesting depth"
  (`io.rs:99`) to also mean "this turn's IO is captured," adds a fourth
  interacting condition to the heuristic that already produced three regressions,
  and does not touch the stdin half.
- **Conflate `TerminalAccess::Denied` with empty stdin.** Rejected: it fixes
  exarch but breaks the policy vocabulary. A piped `ral -c` should be denied
  foreground authority while still reading its pipe. Foreground authority and
  byte input are separate effects.
- **Delete `JobControl` in the lease parcel.** Rejected: after the handoff door
  moves to `&TerminalLease`, a role fact remains — top-level launch versus
  pipeline stage — and still affects `PgidPolicy`. Rename/narrow it later rather
  than losing that invariant.
- **Explicit per-turn foreground policy, but keep `startup_foreground` /
  `JobControl` as authority.** This is a way-station, not a rejection. It fixes
  the live bug, but leaves the two inference sites and the `tui_active` exception
  standing, so the drift-between-paths failure mode survives. The lease is this
  idea carried to the point where the ambient inputs are deleted.
- **Defensive `tcgetpgrp == getpgrp()` guard before `ct_read` in exarch.**
  Legitimate belt-and-suspenders hardening (the TUI input loop arguably should be
  SIGTTIN-robust regardless), but it papers over the symptom — it does not stop
  the wrong handoff. Keep as optional hardening *in addition to*, never instead
  of, the lease.

## What changes, what stays

- **New:** `TerminalLease` (`core::process`), session-owned
  `Option<TerminalLease>`, host-facing `RequestedTerminalAccess` and `TurnStdin`
  on `TurnRequest`, internal `TerminalAccess` on `TurnState`, a `Source::Empty`
  variant (`source.rs` has no null source today), and an explicit loan guard for
  `_ed-tui`.
- **Deleted:** `tui_active` and its `repl.rs` STT plumbing; the `resolve.rs:320`
  capture exception; `startup_foreground` as handoff authority once the lease
  door is in place.
- **Retained first:** `JobControl`'s process-group role; later narrowed or
  renamed to `LaunchRole` after terminal authority is removed from it.
- **Narrowed:** `resolve_terminal_plan` to the lease + final-sink/loan rule;
  `try_acquire` to take `&TerminalLease`; `ForegroundDecision` to consult the
  lease/access policy instead of `job_control` + `startup_foreground`.
- **Unchanged:** the REPL startup terminal claim, `ForegroundGuard`'s
  pgid/termios save-restore and SIGTTOU mask (`signal/unix.rs:613`), the
  pipeline group/anchor/relay machinery, parking-on-stop staying REPL-only,
  `capture_depth`'s `Seq`-flush role, and the helper protocol on every platform.

## Consequences

- The terminal-foreground decision has one source of truth (the lease at the
  door), so the standalone and pipeline paths cannot disagree about which shells
  own the terminal — the class behind all three recorded regressions closes.
- An exarch tool turn that foregrounds a child is no longer a bug to guard
  against; it does not typecheck at the handoff door.
- An exarch tool turn that reads stdin sees an explicit empty source, not the
  controlling terminal; this is a separate `TurnStdin` guarantee, not a side
  effect of foreground denial.
- `tui_active` disappears; `_ed-tui` states its terminal borrow positively.
- [[decisions/260613_terminal-foreground-ownership|terminal-foreground-ownership]]
  becomes *superseded* when this lands: its predicate is the lease's mint
  condition, its regimes are requested terminal access + internal terminal state
  plus `TurnStdin`, and its SIGTTOU-mask restore is retained verbatim.

## Recorded deviation: the `_ed-tui` loan is a manual token, not RAII

The landed implementation deviates from the §4 sketch in one respect, now
recorded accurately. The loan is a *manual* token, not the drop-restoring guard
the §4 sketch (`let _loan = terminal_lease.loan()`) implies:
`Shell::begin_terminal_loan` (`core/src/types/shell/host.rs`) raises the
installed turn to `TerminalAccess::ExplicitLoan` and returns a `TerminalLoan`
carrying the prior access; restoration is the caller's explicit
`end_terminal_loan`, and `TerminalLoan` has no `Drop`.

- **The elevation door is closed.** `begin_terminal_loan` only elevates an
  already-`Leased` turn — `if matches!(prev, TerminalAccess::Leased) { … =
  ExplicitLoan }` — so a `Denied` turn that happens to sit on a session owning
  the lease is left at `Denied` and never reaches the handoff. The loan can only
  *raise* an authorised turn; it can no longer mint authority from `Denied`, the
  §4 invariant the type now enforces.
- **Manual restore, retained intentionally.** The token is still begin/end, not
  `Drop`-based RAII. Without `Drop`, an early return between begin and end would
  leak `ExplicitLoan` until turn teardown. This is acknowledged and accepted: the
  sole caller is the REPL editor builtin (`ral/src/repl/plugin_ed_builtins.rs`),
  which runs inside a `Leased` interactive turn and always pairs
  `end_terminal_loan`, so the leak is unreachable in practice.

A literal RAII guard is awkward in Rust here: a `Drop` impl cannot hold the
`&mut Shell` it needs to restore access while the editor body also borrows
`&mut Shell`. That is why the token stays manual rather than becoming a
drop-restoring guard.

## Implementation plan

Documentation of intended work, not a commitment to build now. Six parcels; each
compiles and tests alone. Parcels 1–3 are the high-value path and resolve the
live crash plus stdin stealing; parcels 4–6 pay down the three-regression debt.

```
1  Token+door   add session-owned Option<TerminalLease>; no public mint/Clone/Copy;
                change ForegroundGuard::try_acquire to require &TerminalLease;
                thread the borrow to standalone, pipeline finish, and fg-resume.
                Behaviour identical.
2  Access       add RequestedTerminalAccess to TurnRequest and TerminalAccess to
                TurnState; REPL and terminal scripts request Leased, exarch
                requests Denied, no-lease launches install Denied; resolve_terminal_plan
                and ForegroundDecision consult the installed access.
3  Stdin        add Source::Empty (source.rs is {Terminal,Pipe,File} today — no
                null source exists); add TurnStdin so Capture no longer implies
                Source::Terminal; exarch passes Empty, ordinary batch/REPL paths
                pass Inherit. Fixes tool stdin stealing without breaking piped ral -c.
4  Loan         add ExplicitLoan + host loan guard; convert _ed-tui; delete
                tui_active, its repl.rs STT plumbing, and the resolve.rs:320
                capture exception.
5  Role         narrow JobControl to process-group role or rename it LaunchRole;
                prove pipeline stages cannot become foreground/new-leader
                orchestrators independently.
6  Cleanup      remove startup_foreground as authority (and as a field if no
                UI/status consumer needs it); collapse resolve_terminal_plan to
                the lease + final-sink/loan rule.
```

## Test plan

- **Regression port.** The three `resolve.rs` foreground tests are rewritten in
  lease terms and stay green: an interactive `Leased` turn foregrounds a
  terminal-bound pipeline; a `Denied` (exarch) turn never does; a terminal-launched
  script (`Leased`) still foregrounds an interactive child; a backgrounded shell
  (no lease) does not.
- **The bug, pinned.** An exarch tool turn running `git diff | from-string` issues
  no `tcsetpgrp` (a `Denied` turn cannot reach `try_acquire`); a tool command that
  reads stdin sees `TurnStdin::Empty`, not the controlling terminal — the
  `shell_eval.rs` harness asserts both.
- **The split, pinned.** A piped `ral -c` can be `Denied` for foreground handoff
  while still reading inherited pipe stdin; denial is not empty input.
- **The capture cases.** Inside a `Leased` turn, `!{ git diff | grep x }` does not
  foreground (buffer sink, no loan); a top-level `claude` does (terminal sink);
  `_ed-tui` running `fzf` does (explicit loan, despite its captured stdout) — the
  CTRL-R regression, re-pinned without `tui_active`.
- **Type-level door.** A compile-fail test (or a `// must not compile` note) that
  `ForegroundGuard::try_acquire` cannot be called without a `&TerminalLease`, that
  `TerminalLease` has no public constructor outside `core::process`, and that a
  host cannot seed `ExplicitLoan` through `TurnRequest`.
- **Startup separated.** The direct REPL startup `tcsetpgrp` path remains covered
  by its existing job-control startup tests; the lease gates only post-startup
  child/job handoffs.
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
the host-intent seam requested terminal access and `TurnStdin` join),
[[internals/pipeline-execution|pipeline-execution]],
[[map/core/io-process|io-process]], [[map/core/runtime|runtime]],
[[map/repl/jobs|jobs]].
