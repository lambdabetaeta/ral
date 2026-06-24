---
status: active
---

# Exarch's panic-recovery contract for the persistent shell

exarch evaluates each tool call as a ral top-level turn against **one
persistent `Shell`** that survives the whole session. `bus::pump` runs the
worker under `catch_unwind` and, on a caught panic, reports a `worker panicked`
line and lets the REPL continue the session on that same `Shell`. The deep
review (260611 §8, finding X8-panic / recommendation A4) showed this left the
live shell corrupted: a panic mid-eval skips ral's save/restore epilogues, so
the next turn inherits the panicking call's half-applied dynamic context — a
leaked `with_capabilities` grant frame, the hijacked stdout/stderr/stdin tees,
a foreign `surface` sink, a stale script-context location, the already-fired
watchdog cancel-scope, leaked env/cwd overrides, or a deleted handler.

ral itself does not rely on Rust unwinding to restore the dynamic context — its
in-tree justification holds inside the language. exarch is the host that
falsifies it: it is the one embedder that catches the unwind and keeps going.

## The decision

We take **A4 option (a) — the shell is poisoned on a caught panic, and exarch
reconciles it back to the last clean boundary** — rather than option (b)
(convert every ral save/restore pair to RAII). A Rust panic is not a ral escape
and the panicking computation has no meaningful continuation; the cheap,
honest move is to discard its effects, not to unwind them one frame at a time
through core.

The contract has two halves, split by who owns each piece of state:

1. **The IO frame self-heals via RAII.** The stdout/stderr/stdin tees, the
   `surface` host sink, the script-context location triple, and the watchdog
   cancel-scope are all installed by `shell_eval::run_shell`'s own
   `IoGuard`, whose `Drop` restores every field. These are host-owned, installed
   per tool call, and never meaningful past the call — so a guard is exactly
   right, and it fires on the normal return *and* on unwind, before the stack
   leaves `run_shell`. (Mirrors core's existing `capture.rs` / `RedirectFrame`
   RAII, converted after an earlier real panic bug.)

2. **The rest of the dynamic context is rebuilt from a durable snapshot.**
   `Session` holds a `durable: Mobile` field, refreshed inside the worker — in
   `Session::run_shell`, right before each `shell_eval::run_shell` dispatches —
   so it always carries the `mobile` that *completed* tool calls left behind and
   never the one a call is mid-way through. The field lives on `Session`
   precisely so it survives `pump`'s `catch_unwind` boundary: the worker writes
   it, and after `pump` returns `Ok(None)` (the caught-panic signal)
   `Session::run_turn` reads it and assigns
   `self.shell.mobile.context = self.durable.context.clone()`. Completed calls'
   bindings (in `mobile.scope`) and cwd (in `context.cwd`) survive; the
   panicking call's grant frame, env/cwd, and handler-stack mutations roll back.

`run_turn`'s existing single-exit `quiesce` then winds the event log back to
`ReadyForUser` ([[invariants/turn-ends-ready]]), so the next prompt is
admissible against a shell whose dynamic context is clean.

## Where

- **`exarch/src/shell_eval.rs`** — `IoGuard` (install + `Drop`); the per-call IO
  frame.
- **`exarch/src/session.rs`** — the `durable: Mobile` field, its refresh in
  `run_shell`, and the rebuild in `run_turn` on `pump` → `Ok(None)`.
- **`exarch/src/bus.rs`** — `pump`'s `catch_unwind`, returning `Ok(None)` on a
  caught panic.

## Covered

`session::tests::worker_panic_preserves_completed_bindings_and_clean_context`:
a scripted run whose first tool call binds a name and whose second panics
mid-eval — the binding survives, the grant stack is back at its baseline depth,
the session is `ReadyForUser`, and a third turn completes on the healed shell.

## The hard rule

Any state the host installs onto the persistent shell for the duration of one
tool call must restore on unwind, not only on the normal return: install it
through an RAII guard (the IO frame) or reconcile it from the durable `Mobile`
snapshot at the `run_turn` panic boundary (the dynamic context). Never add a
hand-written restore epilogue that a panic between install and restore would
skip.

See also [[map/exarch/shell-eval]], [[map/exarch/agent]],
[[map/exarch/frontend]] (the event-log state machine), and
[[invariants/turn-ends-ready]].
