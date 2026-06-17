---
status: active
---

# The pure pipe equation

**`x | f = f !{x}` holds unconditionally at every value edge.** A pipeline's
value edge is data-last application, full stop: the consumer receives the
produced value whole, whatever its shape. Step streams are ordinary recursive
variants, eliminated explicitly by the prelude combinators (`stream-each`,
`stream-map`, `stream-fold`, `stream-to-list`) — never iterated implicitly by
the pipe ([[design/pipelines|pipelines]], [[design/codecs|codecs]]).

Previously the edge dispatched on the value's *structure*: a runtime
recognizer (`try_drive_stream`) drove the consumer once per element when the
upstream matched `` `more [head, tail: {…}] `` or carried a `` `done `` label,
and the checker mirrored it with a probe inside `apply_piped_value` that
substituted the element type for the stream type. Both are deleted; one
β-rule remains.

- **Two bugs motivated the deletion, and the deletion is the fix.** (1) A
  soundness hole: the runtime's `` `done `` arm returned `Unit` while the
  checker had typed the edge at the element type, so a `` `done ``-terminated
  pipe could hand downstream stages a `Unit` the types never admitted. (2) A
  payload-carrying `` `done 5 `` was swallowed silently — the consumer never
  ran. Under the equation both programs mean exactly what application means.
- **The checker keeps one diagnostic trace of streams**: when a piped variant
  carrying `` `more ``/`` `done `` clashes with the consumer's parameter, the
  unification hint points at the explicit eliminators. Zero semantic effect.
- `Step` survives as the codecs' data type (`from-lines` still returns the
  recursive `` `more {head, tail: Thunk(F Step)} | `done `` variant typed by
  `lines_step_ty`); only the pipe's special treatment of it is gone.
- Two adjacent wire bugs were fixed alongside: a consumed stage's annotation
  now reports input `∅` with its application body's output mode (so `return
  [1, 2] | { |xs| echo $xs } | wc -l` wires `Bytes` to `wc` instead of
  tripping the adjacency assertion), and a process-staged helper forces a
  bare-block producer's value once before it crosses the value channel,
  mirroring the parent-side fold's single force.
- The co-inductive unification that lets value-anchored and comp-anchored
  stream types meet — what makes the explicit eliminators ergonomic — is
  [[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]].
