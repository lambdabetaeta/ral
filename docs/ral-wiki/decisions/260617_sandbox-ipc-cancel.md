---
status: active
---

# Sandbox IPC cancellation belongs to the parent

**A confined eval child can be busy while the parent is blocked in IPC, so the
parent must own the out-of-band teardown.** Deadline and Esc cancellation live on
the parent foreground `CancelScope`; they do not travel through a synchronous
frame read unless the parent has a side channel that can kill the confined helper
and its descendants.

## The incident

An exarch `shell` tool call ran under the normal grant-framed sandbox and issued a
long `cargo test` command. The call crossed its 30 s wall-clock limit, but the TUI
Esc path did not return control. Killing exarch externally made the session
usable again because the sandbox IPC edge closed and the parent could finish
classifying the interrupted turn.

The session artifacts showed the wrong layer trying to act:

- the exarch parent had armed the per-call deadline scope;
- `run_confined` was blocked in the sandbox IPC round trip, waiting for a child
  frame or EOF;
- the macOS Seatbelt log reported `deny(1) signal children ... signum:2` from a
  descendant test process;
- Esc/deadline set cancellation state, but no parent-owned waiter observed it
  while the parent was synchronously blocked on IPC.

The Seatbelt denial is useful evidence but not the root fix. Granting more signal
authority to the confined child would only help the specific child process that
asked to signal its descendants. It would not make the parent’s timeout or Esc
able to break a wedged IPC round trip.

## Decision

`run_confined` starts a parent-side `SandboxCancelWatch` for each Unix sandbox
helper.

- The watcher borrows the same foreground `CancelScope` the enclosing turn uses.
- While the parent waits on the sandbox IPC channel, the watcher polls the scope
  cause.
- On `Interrupt`, it sends `SIGINT` to the observed helper subtree.
- On `Deadline` or `Explicit`, it sends `SIGTERM`, waits briefly, then sends
  `SIGKILL` to the observed subtree.
- On `RootAbort`, it sends `SIGKILL` immediately.

The kill happens outside the OS sandbox, from the still-unconfined parent. That
keeps the sandbox profile narrow: the confined child does not gain general
`signal children` authority merely so the host can enforce its own wall clock.

The host-facing classification stays cause-aware. A deadline maps to tool exit
124; an Esc remains an interrupt, not a timeout. The two causes share the same
teardown machinery but not the same user-visible meaning.

## Where

- [`core/src/sandbox/runner.rs`](../../../core/src/sandbox/runner.rs) —
  `run_confined` spawns the sandbox helper, starts `SandboxCancelWatch`, and
  tears down the observed subtree on parent-scope cancellation.
- [`exarch/src/shell_eval.rs`](../../../exarch/src/shell_eval.rs) — `run_shell`
  maps only `CancelCause::Deadline` to timeout exit 124, and keeps Esc distinct.
- [[internals/capability-enforcement|capability enforcement]] — the operational
  narrative for re-execed sandbox evaluation.
- [[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]] — the parent
  foreground scope that supplies the cause.

## Covered

- `shell_eval::tests::timeout_kills_sandboxed_subprocess_tree` — a sandboxed
  `/bin/sh` forks a sleeping grandchild and waits; a 2 s exarch timeout returns
  promptly with exit 124 and the grandchild gone.
- `shell_eval::tests::timeout_kills_external_subprocess_tree` — the existing
  non-sandboxed process-group timeout still tears down forked descendants.

## The hard rule

Any host that waits synchronously on a confined child must have an out-of-band
parent-side cancellation path. Cooperative polling inside the parent is not a
cancellation boundary if the parent is blocked in IPC, and extra authority inside
the child is not a substitute for host-owned teardown.
