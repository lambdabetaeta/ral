---
status: accepted
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

## Resolution (as built)

The open questions resolved as follows; the two prior cutovers had already made
the turn-assembly names un-callable, so this pass was the enforcement-and-tidy
it set out to be, not a re-architecture.

1. **Close the old core exports — done, the aggressive cut.** `core/src/lib.rs`
   no longer re-exports the evaluator/syntax/IR layer: `apply`, `evaluate`,
   `elaborate`, `parse`, `ParseError`, `Comp`, `Val`, `Ast`, and `SourceLoc`
   leave the crate root. The ones core still needs internally
   (`elaborate`/`evaluate`/`parse`/`ParseError`/`Comp`, used by `compile` /
   `compile_and_typecheck`) became `pub(crate) use`; the rest were dropped. A
   host or test that needs raw compile/eval reaches the owning module
   (`ral_core::syntax::parser::parse`, `ral_core::evaluator::evaluate`,
   `ral_core::ir::Comp`, …), which now reads as deliberately stepping past the
   `run_turn` seam. The crate root keeps the seam (`run_turn`, `TurnRequest`,
   `TurnIo`, `TurnReport`, `Captured`, `StaticDiagnostics`, `TurnLifecycle`,
   `SurfaceSink`/`EventSink`), the typed-compile API
   (`typecheck`/`Scheme`/`SessionSchemes`/`TypeError`/`bake_prelude`), the
   host-embedding re-exec helpers, and ordinary value/rendering types.
2. **Tests on the public seam — already held.** `core/src/turn.rs`'s
   host-behaviour tests drive `Shell::run_turn`; the eval-semantics integration
   tests (`sandbox_fail_closed`, `scope_escapes`, …) legitimately use the
   evaluator entry `eval_top_level`, which is core mechanics, not a host turn.
   No host crate assembles a turn by hand — `ral/tests/seam_guards.rs` enforces
   it.
3. **`shell_eval.rs` kept.** It is still a real boundary, not an empty adapter:
   it owns `AgentSink` (the `EventSink` impl) and `value_to_kind` (the rail
   vocabulary the host-loop ADR keeps in exarch). Per this decision's own
   conservative rule it stays; folding it into `session.rs` or renaming to
   `tool_turn.rs` would not have removed a boundary, only moved a name.
4. **Shared turn driver — done.** The explicit-done completion contract is one
   primitive, `bus::drain_pass`, called by both the headless default
   `Sink::drive` and the TUI's `drive_events`; the only difference is the frame
   timer (the default blocks on the channel between passes, the TUI renders and
   polls keys). Completion stays "the worker set `done`," never channel
   disconnect, in one place the two drivers cannot drift on. Pinned by
   `bus::tests::drain_pass_stops_on_done_with_a_live_detached_sender`.
5. **Host accessors narrowed.** `Shell::durable_root` is `pub(crate)` — only
   `run_turn` mints the foreground scope now, no host does. The others have live
   host uses and stay: `foreground` (exarch cancel-poll tests), `stderr_mut`
   (exarch diagnostics), `set_stdout` (the REPL's ambient external printer),
   `sources`/`terminal`/`is_interactive`/`enable_audit`/`take_audit_fragment`/
   `builtin_names`/`repl`/`repl_mut`.
6. **`session::TurnOutcome` kept.** With core `TurnOutcome` gone there is no
   collision, so the provider-round-trip name is unambiguous in context; the
   `ProviderTurnOutcome`/`AgentTurnOutcome` rename was not worth the churn.

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
