---
generated_at_commit: d56977ff
generated_at_date: 2026-11-03
covers_paths: [core/src/shell/evaluator.rs, core/src/shell/evaluator/]
---

# Map: core / shell / evaluator

**`core/src/shell/evaluator/` is the CEK machine for the checked CBPV IR.**
The [[internals/evaluator-machine|machine narrative]] describes its rules;
[[invariants/ir-pure-cbpv|the IR invariant]] partitions control, primitives
and shell. Hosts enter only through the framed run in `shell/run.rs`.

## Entry

- `machine::evaluate(comp, env, mooring, shell)` injects ⟨M, ρ⟩ over an
  empty stack. `machine::apply(f, args, …)` pushes the arguments, if any,
  then forces the value. Both are crate-private and return `Settled<Value>`.
- `run_phrases` (`shell/evaluator.rs`) runs `Phrase::{Define, Run}` in
  order. A landed `Define` immediately extends the environment; under
  `Mode::Session` it also updates `shell.env` and the lease ledger.
  Earlier definitions survive a later failure.
- Higher-order natives re-enter through `machine::apply`; session hooks
  through `evaluator::apply`. `NESTED_MACHINE_LIMIT` bounds host-stack
  nesting independently of the machine's frame cap.

## Machine: `machine.rs`

- `Focus::{Eval, Return, Halt}` holds a computation closure, a terminal,
  or a `Break`. `Terminal::{Value, Lambda}` distinguishes a returned value
  from a lambda computation awaiting its argument.
- `eval_rules` has one arm per `CompKind`; `step_return` and `step_halt`
  have one per `Frame`. Rules return `Settled<Focus>`; raising points use
  `stamp`, and `step` folds failure into `Halt`.
- `App` pushes one `Frame::Arg` per argument, right to left, then evaluates
  the head. Only `Lambda` consumes `Arg`, binding its one name and entering
  the body. Returned-thunk splits follow the [[invariants/ir-pure-cbpv|IR
  invariant]]. A call with n arguments needs n free frames.
- `force_val` enters a literal thunk in the current environment without
  closing it. `force` enters a stored thunk's closure. `native_code` builds
  the lambda chain ending in `CompKind::Native` when a native is forced;
  received arguments live in the partial closure's environment. Full-arity
  resolved `Exec` calls run the builtin directly.
- `Bind` pushes `Frame::To`; `Seq` pushes `Frame::Seq`. These alone carry
  saved environments for their continuations. A lambda binds a name;
  parameter destructuring runs through body binds at parameter spans.
- `If` forces its selected arm; `step_case` first pushes one `Arg` holding
  the variant payload. Bare tags carry `Unit`. `Try` passes an error record
  to its handler by the same push-then-force rule.
- `Capture` swaps stdout for a buffer. Success restores the sink and returns
  exact `Bytes`; overflow refuses a prefix. A halt restores the sink and
  drops the buffer.
- `Rec` unfolds the selected member of a shared `GroupNode`. Every sibling
  closes over one environment restricted to the group's occurrence set;
  recursive references re-enter through force without rewriting bodies.
- Every frame is pushed through a `Slot` that `reserve`/`reserve_n` checked
  against `session.stack_limit` before any effect. `push_args` reserves all n
  `Arg` frames at once. Consuming `Arg` enters the body in tail position;
  no consumed call frame remains.
- `Frame::abandon` supplies undo to the rules and panic walk. Only `Seq`
  and a written `let` add the abandoned-steps hint; compiler-written binds
  do not add it merely for abandoning an operand or split call.
- Shell rules dispatch `Exec`, launch and join a `PipeNode` for `Pipeline`,
  and birth workers for `Spawn`. `Frame::Gather` owns a `ScopeOwner` and
  restores its displaced cradle before the shared brood-exit machinery
  joins or stops children. See [[internals/pipeline-execution|pipelines]]
  and [[invariants/a-worker-ends-with-its-children|worker scope]].

## Values and primitives

- `val.rs`: `form(val, env, sig)` is side-effect-free. Variables use
  `lookup`; thunks use `Closure::new`; variants use `Value::variant`,
  normalising unit payloads. Plain list/record/map literals close over
  precomputed occurrence sets, unless a directly mentioned name requires
  Σ lookup and therefore eager formation.
- `expr.rs`: `eval_prim` interprets every `Prim` on values. `field`,
  `position` and `lookup` implement `Key::{Label, Position, Data}` reads;
  `position` is shared with `nth`. `Render` checks scalars; `Concat` joins
  strings. `Decode` drops its environment before taking over the bytes,
  removes one trailing terminator and checks UTF-8 strictly.
- `assemble.rs`: `eval_list` splices list values, `eval_update` replaces
  fields over one record base, and `eval_map` assembles map values. All
  operands have already run in surrounding binds. Explicit map entries
  beat spreads, first spread wins, and a repeated computed key fails here
  after every operand ran; known duplicates are refused statically.
- `pattern.rs`: `destructure` matches `Pattern::{Wildcard, Name, List,
  Record}`, collecting all bindings before installing any. A list without
  rest requires exact length. Mismatches are catchable errors; lambda
  parameter failures point at the parameter's span.

## Shell interpretation

- `scope.rs` installs dynamic frames. `WithinScope` forms the option record
  and handlers; `WithinUndo` restores cwd, env overrides and handlers.
  Grants attenuate authority; cleanup restores the body's outcome unless
  cleanup itself fails. Handler dispatch is
  [[internals/handler-dispatch|deep and self-masking]].
- `call.rs`: `run_native` runs each native body inside `BuiltinEntry::framed`.
  The builtin may re-enter the machine; it is not a value-only primitive.
- Redirect installation and capture helpers live in
  [[map/core/runtime|shell/exec]] (`RedirectState`, `with_redirects`,
  `with_capture`). `STAGE_EVAL` supplies `evaluate` and `close_argv` to
  pipeline execution; a ral stage starts a fresh machine on its thread.
- `Val::Protected` wraps delayed obligations through `Site::wrap` without
  admitting data at import. `CompKind::Protect` pushes `Frame::Protect`;
  its return admits and wraps. Handle guards apply at exposure, never by
  consuming a handle at import ([[map/core/typecheck|typecheck]]).
- `Tilde` reads live `HOME` through `Context::require_home`. Ambient reads
  are natives; `$SCRIPT` is a literal baked by elaboration.

The loop catches panics and abandons frames; the framed run restores its
checkpoint. Hot paths poll cancellation cooperatively. See
[[map/core/shell-state|shell state]], [[map/core/runtime|runtime]].
