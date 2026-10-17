---
status: active
generated_at_commit: dad6a1b3
verified_at_commit: dad6a1b3
anchors: [Frame::Cleanup, Shell::shield, Scope::shielded, cleanup_grace, CollectState::cancel_all]
---

# A cleanup is shielded within its grace

**A `guard` cleanup runs under a shielded child scope that masks every stop
from above, bar `Aborted`, until a grace counted from the strike has passed.**
A stop is level-triggered
([[decisions/261016_delimiters-are-handlers-for-poll|delimiters-are-handlers-for-poll]]):
without a mask, a cleanup entered on a stop would be stopped again at its
first poll, so a cleanup under Ctrl-C could run no command, no pipeline and no
loop. Successor to
[[decisions/260819_a-failing-cleanup-pre-empts|a-failing-cleanup-pre-empts]],
whose precedence rule stands unchanged.

## Decision

- **The law.** `guard (return V) N ≃ shield (N ؛ return V)` and `guard (halt
  σ) N ≃ shield (N ؛ halt σ)`: the finaliser *and the re-halt* run under the
  shield, as Haskell's `bracket` puts both under `mask`, so no checkpoint
  falls between consuming the halt and raising the shield. `shield N`
  completes, or halts on `stop Aborted`, or halts on `stop j` once the mask
  has ended. A halt of `N` still pre-empts (260819).
- **The shield is a node pushed by a frame.** Trio's shape, taken literally:
  both `Frame::Guard` arms call `Shell::shield(mooring.cleanup_grace)`, which
  swaps `Shell::scope` for `scope.shielded(grace)` and hands back the previous
  scope; `Frame::Cleanup` restores it on pop and on abandon, as
  `Frame::Capture` restores `io.stdout`. Everything launched inside the
  cleanup — a pipeline's stages, an external child, a nested machine such as
  `map` applying a function — reads the current scope and is masked with it,
  with no code of its own.
- **What it masks.** The shielded node's fold admits from its ancestors only
  `Aborted` and, once the mask has ended, everything. It is minted as a `Scope` alone, so its own flag is written
  by nobody, so a descendant's own cause (a reader-gone stop of a stage inside
  the cleanup) is never masked. It acts on the chain, before the join, so a
  descendant's cause stays visible under a masked ancestor's stronger one.
- **The budget belongs to the strike.** A node records the instant its flag
  first rose (`cancelled_at`), written by its one writer in the act that
  raises it. A shield masks while `now < since + grace`, where `since` is the
  earliest strike on the chain above it and `grace` is the host's policy
  (`Mooring::cleanup_grace`, from `Run::cleanup_grace`, five seconds by
  default), capped by the nearest enclosing shield's grace. Nothing is
  counted from the cleanup's own start, nothing is inherited from a popped
  frame, and nothing on the node is mutated by a read.
- **`Aborted` passes every shield**: a defect is not negotiated with, and
  Ctrl-`\` is the key that insists.
- **The shield is cooperative.** It keeps ral from stopping or killing the
  cleanup for its grace. Signals the kernel or the pipeline's owner sends to
  the process group (the tty's SIGINT, the collector's one grace signal) still
  reach a cleanup's children, as under any shell, and a child that dies of one
  is a process failure (`` `signaled n ``), since the current scope carries no
  cause to attribute it to. The stop stays in force on the node, and the
  cleanup's first poll after the grace reads it. The loan strikes the *node*;
  attribution reads the *current scope*; under a shield the two disagree by
  design.
- **The collector honours the shield.** After its signal phase,
  `cancel_all` waits for live *thread* stages, bounded by the grace, before
  the group kill, since a thread stage's cleanup children share the group. A
  stage panic or a root abort grants no grace; a panic or a stronger cause
  read mid-teardown is written over every live stage at once, and at
  `Aborted` the waits end. Every teardown follows a reading of the current
  scope, so a pipeline inside a cleanup is torn down only once the shield has
  ended.

## Consequences

- Every cleanup of one unwinding, nested or successive, across frames and
  threads, ends by one instant, `since + grace`; no sequence of finalisers can
  multiply the budget.
- A cleanup already running when the stop lands gets the whole grace from the
  strike, however long it has run; one entered with a stop pending gets what
  remains; one entered after the grace is spent is stopped at its first poll.
- A run ends within `wall + grace + TEARDOWN_GRACE`, plus the step in
  progress and the slice. This is a construction: no node can mask past
  `since + grace`, and `since` is at latest the wall's firing.
- A nested run (a hook, an aside) is dispatched under its host's scope, never
  under `Shell::scope`, so one dispatched from inside a cleanup is not
  shielded with it. A worker wipe's drain (`WORKER_DRAIN_GRACE`) is sized to
  the default grace; a host granting a longer one accepts that a wipe may cut
  it.

## Rejected shapes

- **A grace counted from the cleanup's start, inherited from the live
  ancestry.** A cleanup struck after `grace` had elapsed since its own start
  was stopped at its next poll, the inverse of the case the shield exists for,
  and each successive finaliser got a fresh grace.
- **Counters and a clock on the shared node**, mutated inside the fold. The
  one instant recorded is written once, by the node's one writer.
- **A grace per node, started at the first stop over a cleanup.** Nested
  nodes each started a grace when the ancestor's ended: the bound was `wall +
  depth × grace`.
- **An `abort` flag in the collector's teardown.** What `Aborted` changes is
  that no shield masks it and no grace signal precedes the kill, so the waits
  end on their own.
- **Signalling externals by pid while a thread stage lives.** A command inside
  a thread stage leaves its grace signal to the group's owner, so by-pid
  delivery left it unsignalled. The group is signalled once; the shield's
  promise is cooperative.
- **Forgive whatever a reader-gone stage raises.** It contradicts the law
  above: a cleanup's own halt is its own.

## Where

`Frame::Guard` and `Frame::Cleanup` in `core/src/evaluator/machine.rs`;
`Shell::shield` in `core/src/types/shell/mod.rs`; `Scope::shielded` and
`resolve` in `core/src/process/cancel.rs`; `Mooring::cleanup_grace`;
`Run::cleanup_grace` in `core/src/protocol.rs`; `CollectState::cancel_all` in
`core/src/runtime/pipeline/collect.rs`. See
[[internals/cancellation|cancellation]].
