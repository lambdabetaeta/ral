---
status: open
---

# After the turn API cutover, simplify by enforcing turn boundaries

**Once [[decisions/260618_run-turn-host-loop|run-turn-host-loop]] and
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]] land, the next
simplification is not another abstraction. It is a visibility cut: make the old
turn seams impossible to call, then delete the adapter code that becomes empty.**
The rule is conservative: remove names that only existed to let hosts assemble a
turn; keep names that still describe exarch presentation, durable byte IO, or
REPL ambient setup.

## Architectural diagnosis

There is no second burn-it-down error behind the run-turn work. The large
mistake is one family: **turn-local facts leaked into long-lived or host-owned
machinery**.

- Completion leaked into presentation transport: channel disconnect became "turn
  done."
- The structured surface leaked into persistent shell state: a turn-local effect
  sink was cloned into detached workers.
- Materialised evaluator resources leaked into host code: `TurnFrame` and
  `IoFrame` became public host vocabulary.

The fix is therefore not to invent a larger controller. It is to restore the
boundaries the types already want: a turn is an explicit call; hosts supply
policy and render reports; core materialises resources; exarch presentation
stays presentation. The cleanup below is the enforcement pass for that
diagnosis.

## Draft decision

The follow-on cleanup runs in this order.

1. **Close the old core exports.** `ral_core::lib.rs` stops re-exporting
   evaluator/frame internals. Hosts should import only `Shell::run_turn`,
   `TurnRequest`, `TurnIo`, `TurnReport`, `SurfaceSink`/`EventSink`, lifecycle,
   capture, diagnostics, and ordinary value/rendering types.
2. **Move tests to the public seam.** Tests that assert host-visible behaviour
   call `run_turn`; only core guard/signal-slot tests use private helpers.
3. **Shrink exarch's ral adapter.** `exarch/src/shell_eval.rs` becomes a small
   `TurnRequest` builder plus `TurnReport` renderer. If it stops carrying a real
   boundary, fold it into `session.rs` or a narrowly named `tool_turn.rs`.
4. **Share the exarch turn driver.** TUI and headless use the same explicit-done
   loop; the only policy difference is renderer and frame timer.
5. **Narrow host accessors.** After frame assembly leaves hosts, re-check
   `durable_root`, `foreground`, `stderr_mut`, `set_surface`, and friends.
   Delete accessors whose only remaining job was old frame construction; keep
   ambient REPL byte setup (`set_stdout`) unless `TurnIo` later grows an explicit
   live-printer case.
6. **Rename exarch's provider outcome if it still reads ambiguously.** With core
   `TurnOutcome` gone, `session::TurnOutcome` may remain, but
   `AgentTurnOutcome` or `ProviderTurnOutcome` better states its layer.

## Guardrails

Do not collapse distinct channels just because the code is shorter:

- Keep exarch `Event`/`Kind`: they are the presentation vocabulary, not turn
  liveness.
- Keep bytes and surface separate: `watch` needs durable byte sinks; surface is a
  turn-local structured effect.
- Keep `set_stdout` until the host API has an equally explicit way to install the
  REPL's ambient external printer.
- Do not move tokio into `ral_core` while sharing the driver.

## Open questions

- Does `shell_eval.rs` disappear into `session.rs`, or does a small
  `tool_turn.rs` make the adapter boundary clearer?
- Is `ProviderTurnOutcome` worth the churn, or does deleting core `TurnOutcome`
  make the remaining name unambiguous enough?
- Which `Shell` host accessors survive once all hosts are request suppliers?

## Test notes

The cleanup is done only when grep guards pass:

- no host names `TurnFrame`, `IoFrame`, core `TurnOutcome`, public `eval_turn`, or
  `arm_lifetime`;
- `ral_core` names no `tokio`, `spawn_blocking`, `mpsc`, or `select`;
- exarch TUI and headless exercise the same explicit-done driver in tests.

See also [[decisions/260618_run-turn-host-loop|run-turn-host-loop]],
[[decisions/260618_run-turn-is-host-api|run-turn-is-host-api]],
[[decisions/260616_unify-turn-evaluation|unify-turn-evaluation]], and
[[map/exarch|map: exarch]].
