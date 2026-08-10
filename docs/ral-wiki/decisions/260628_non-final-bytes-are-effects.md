---
status: superseded
superseded_by: decisions/260809_pipes-are-positional-byte-wires
---

# A value boundary binds the final value; non-final bytes are effects

**At a value boundary — a `let`, a forced block `!{…}`, or a `try` outcome — the
bound value is the *value of the boundary's final computation*. Bytes written by
the non-final commands of a sequence are effects: they flush to the visible outer
stream and are never folded into the bound value. Only when the final computation
is byte-output with a `Unit` value are its bytes the value, decoded as a `String`
after stripping one trailing newline.** The runtime and the checker compute this
"final output" the same way, so the static type of a binding matches the value the
evaluator produces.

## The decision

A sequence `{ a; b; c }` evaluates to the value of `c`. What `a` and `b` wrote to
stdout already happened — it is past, visible output, not a candidate value. So
`let x = !{ echo log; length xs }` binds the `Int` from `length`; `echo log`'s
line is an effect on the terminal, not part of `x`. A binding observes one
computation's value, and a sequence has exactly one final computation.

- **Sequencing flushes non-final bytes.** `eval_seq` runs each non-final element
  under a non-tail continuation and, when a capture is installed, flushes that
  element's buffered stdout to the saved outer sink before the next element runs
  ([`eval_seq`, `core/src/evaluator/comp.rs`](../../../core/src/evaluator/comp.rs)).
  The capture buffer therefore holds only what the *final* element wrote, so
  draining it yields the final byte value.
- **The bind is uniform.** `eval_bind_rhs` captures stdout iff the RHS is
  byte-mode; a `Unit` result decodes the drained bytes (one trailing newline
  stripped), any other result binds directly
  ([`eval_bind_rhs`, `core/src/evaluator/comp.rs`](../../../core/src/evaluator/comp.rs)).
  `!{…}` no longer carries a special "capture everything" path — forcing a block
  is just a byte-mode RHS like any other ([[decisions/260616_force-eliminates-blocks|force-eliminates-blocks]]).

This restates, for the general sequence, the rule §4.3 already states for the
line editor's `_ed-tui` capture and §4.2 for return types: *a non-`Unit` value
wins outright; otherwise the captured bytes are the value.*

## Why not capture all stdout

The rejected alternative made `!{…}` capture *every* byte the block wrote and
decode the lot — `let x = !{ echo one; echo two }` would bind `"one\ntwo"`. It
reads well for the all-`echo` block but is wrong as a principle:

- It conflates *effect* with *value*. `echo log` in `!{ echo log; length xs }` is
  a deliberate side-effect; capturing it discards `length`'s actual return value.
  The construct that means "collect a command's output as a value" is the pipe
  and the codec ([[design/codecs|codecs]]), not the act of sequencing beside it.
- It needed a runtime special-case — a `force_capture` flag threaded into the
  bind, switched on the RHS being a `Force` and the pattern not being a gensym —
  to keep value-position `!{…}` (inside a map literal, say) producing `Bytes`.
  That flag is a symptom: a rule that needs to know whether its binder was
  user-named is not a rule about values.
- It put the checker and the evaluator at odds. The type of `!{ echo hello;
  length [1,2,3] }` is `Int` by the return-type rules of §4.2, but the
  capture-all evaluator returned the decoded `String`. The bad state — a binding
  whose static type is not the value it holds — was reachable.

## Static and dynamic agreement: `final_outputs`

The checker must agree with the flush. A node's *effect output* (does evaluating
it ever write bytes?) is not its *value byte-source* (are the bytes the value at a
boundary?). A `Seq` may emit bytes from a non-final command and still return a
clean value. The two facts are kept in separate maps on `InferCtx`: the existing
`bind_outputs` and a new `final_outputs`
([`core/src/typecheck/env.rs`](../../../core/src/typecheck/env.rs)).

- `record_final_output` runs after every `infer_comp` and records the output mode
  of the computation whose value the node returns — the *last* element of a
  `Seq`, the `rest` of a `Bind`, the body of a `Lam`, the join of an `If`'s arms,
  and so on ([`core/src/typecheck/infer.rs`](../../../core/src/typecheck/infer.rs)).
- `observed_value_ty` applies the boundary rule to a type: a byte-output
  computation whose value is `Unit` is observed as `String`; everything else
  keeps its value. The `let` rule grounds the bound type through this, using the
  node's `final_outputs` entry rather than its raw return spec.
- `join_byte_output` is the mode join a branch point needs — `Bytes` on either
  side is `Bytes`, `None` on both is `None` — distinct from `union_mode`'s
  equality-strict unification (§4.2.1), because two arms that *both* write bytes
  are byte-output even though their mode variables never unified.

## `to-line`, `echo`, and `try`

- **`to-line` returns `Unit`.** It writes its argument plus a newline to stdout
  and yields `Unit` — the canonical byte-output-with-`Unit`-value shape, so a
  value boundary captures the line as a `String`
  ([`builtin_to_line`, `core/src/builtins/codecs.rs`](../../../core/src/builtins/codecs.rs);
  `sig::TO_LINE`, `core/src/typecheck/builtins.rs`). The other `to-*` codecs still
  return the `Bytes` they wrote; `to-line` is the line-writer `echo` desugars to.
- **`echo` is sugar over `to-line`.** `expand_echo` lowers to a bare `to-line
  !{ intercalate " " [...] }` with no `Seq[…, Return Unit]` wrapper — the `Unit`
  is `to-line`'s own ([`core/src/elaborator.rs`](../../../core/src/elaborator.rs),
  [[decisions/260622_functions-and-handlers|functions-and-handlers]]).
- **`try` observes both outcomes.** A `try` yields its body's value or, on
  failure, its handler's. Each arm is observed through `observed_value_ty`, so a
  recovery arm that only writes a line — `try { fail … } { |_| echo missing }` —
  has the line as its value, and the two arms must agree on the observed type
  ([`infer_try`, `core/src/typecheck/scope.rs`](../../../core/src/typecheck/scope.rs)).
  The wrapper's own output mode is the byte-join of the arms, so a byte-writing
  outcome is not pinned to `none`.

## Consequences

- `let xs = !{ echo one; echo two; echo three }` binds `"three"`; `one` and `two`
  print. The all-`echo`-block ergonomics are gone, deliberately — collecting many
  lines is `| from-lines` or an explicit accumulator, not a side-effect of `let`.
- A handler can recover by writing a line: `try { … } { |_| echo caught }` binds
  `"caught"` with no explicit `return`.
- The checker and evaluator agree on mixed-mode sequences; the regression is
  pinned by `mixed_sequence_return_type_matches_runtime_value` and its neighbours
  in [`ral/tests/pipeline.rs`](../../../ral/tests/pipeline.rs).
