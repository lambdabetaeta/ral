---
generated_at_commit: ad2123a08dfd12616ada6318e9740042ab7e9e37
verified_at_commit: ad2123a08dfd12616ada6318e9740042ab7e9e37
verified_at_date: 2026-11-03
anchors: [Machine, step_eval, eval_rules, step_return, step_halt, Frame, Focus, Terminal, Closure, Closure::new, Node::new, Env, restrict, Signature, lookup, form, run_phrases, Phrase, evaluate, apply, reserve, PipeNode, NESTED_MACHINE_LIMIT, force, force_val, native_code, Frame::Gather, ScopeOwner, exit_brood]
---

# The evaluator: a CEK machine for CBPV

**The machine steps a computation closure against a continuation.**
`Machine { focus, stack }` is private to `core/src/shell/evaluator/machine.rs`;
it steps against `&mut Shell` and the run's `&Mooring`.

- `Focus::Eval { comp, env }` is ⟨M, ρ⟩, a computation under the whole
  environment in which it was reached.
- `Focus::Return(Terminal::Value(v))` is a returned value.
- `Focus::Return(Terminal::Lambda { comp, env })` is a lambda closure,
  canonical at `A → C`, not a value.
- `Focus::Halt(Break)` is failure or escape climbing the continuation.
  Failure is a register, not an IR term.

The core corresponds to the substitution-based CK machine in
`dev/agda/Extrinsic/Semantics/Machine.agda`. The
[[invariants/ir-pure-cbpv|IR invariant]] partitions control, primitives and
shell; the [[decisions/261102_the-checked-ir-is-the-calculus|correspondence ADR]]
states the standing gaps.

## Application and force

**Only a lambda terminal consumes an argument.** `App` forms its value
arguments, pushes one `Frame::Arg { value, span }` per argument right to left,
then evaluates its computation head. A lambda meeting `Arg` binds its one name
in the closure's environment and puts its body in focus. A returned value
meeting `Arg` is a fault in ral, unreachable for a checked program.

- Calling a value means pushing arguments, then forcing it. This is shared
  by bound heads, handlers, recovery handlers and the host's `apply` entry.
- Forcing `Value::Thunk(Closure)` puts its computation and environment in
  focus, pushing nothing. A literal `Force(Val::Thunk(node))` enters the
  body in the current environment without first closing it (`force_val`).
- Forcing `Value::Native(entry)` builds `native_code`:
  `λx₁…λxₙ. Native(entry, x₁…xₙ)` in the empty environment. At arity zero
  it runs immediately; partial application is an ordinary lambda closure.
  A resolved full-arity `Exec` runs the builtin directly.
- `Case` pushes one argument, the payload, before forcing its selected arm.
  A bare tag carries `()`. `Try` similarly passes the error record to its
  handler on a catchable failure.
- Application through a returned thunk is already explicit in checked IR:
  `M to h. force h · V`, as the [[invariants/ir-pure-cbpv|IR invariant]] requires.
- Elaboration hoists a value head before its arguments; a function-typed
  head is bound unrun and runs when applied.

**Tail entry retains no consumed argument frame.** A call with n arguments
needs n free frames before its head runs; after a lambda consumes its frame,
its body runs in tail position. Every computational frame is pushed through a `Slot` that
`reserve`/`reserve_n` checked against `session.stack_limit` (default 100 000,
set by `--recursion-limit`) before any effect. `push_args` reserves all n
argument frames at once.

## Continuations

**Syntax that resumes later carries its own environment.** Only `To` and
`Seq` carry an `Env`; frames hold shared IR and undo tokens, not `Context`
snapshots.

- `Bind` pushes `To { bind, env }`; on return it matches the pattern and
  extends that saved environment for the rest. `Seq` pushes `{ rest, env }`
  and discards the first computation's returned value. A written `let _`
  still uses `Bind`, so it has the ordinary capture rule.
- Only a written sequence or `let` sets the abandoned-tail fact when its
  continuation is abandoned. Synthetic hoists, returned-thunk splits,
  rendering and capture decoding set none.
- `Redirect`, `Unmask`, `Within`, `Grant` and `Try` pass lambda terminals
  through. Value-demanding frames reject a bare lambda; `let` avoids that
  case by binding a function-typed right-hand side unrun as its thunk.
- `Guard` runs cleanup under a shield; `Cleanup` holds the body's outcome
  and restores it unless cleanup fails. The shield is restored on pop and
  abandonment.
- `Capture` swaps stdout for a fresh buffer and holds the replaced sink.
  Success yields exact `Bytes`; a halt restores that sink and drops the
  buffer, adding the actual discarded byte count to the error's saturating
  unwind total. Escapes carry no unwind facts. Buffer overflow refuses a
  prefix. The surrounding
  bind feeds `Prim::Decode`, which drops one trailing terminator and checks
  UTF-8, with the environment dropped before taking over the byte buffer.
- Shell frames restore their own dynamic state: redirects settle writes,
  `Within` restores env overrides, cwd and handlers, `Grant` pops its layer,
  `Unmask` restores a handler, and `Audit` closes its trail scope.
- `Gather` holds a `ScopeOwner` and the displaced cradle. Its body runs on
  this machine; its exit restores the cradle and joins or stops the brood
  through the shared worker exit machinery
  ([[invariants/a-worker-ends-with-its-children|a worker ends with its children]]).

`eval_rules` has one arm per `CompKind`; `step_return` and `step_halt` have
one arm per frame. Rules return `Settled<Focus>`; raising points locate errors
with one visible operation name/span, and `step` folds the error into `Halt`.
`frame` names that frame (`index`, `fail`, a command head), not necessarily a
command. `Exec`/`App` rely on their innermost dispatch/admission, not fallback
stamps. A `CallSite` frame restores the caller's total provenance register on
return, halt and panic; adjacent tail-call restorations coalesce and are not
counted toward the computational stack cap. Synchronous operations bracket
register changes with `at_site`. A run base completes unclaimed failures, so
the record's site is total, at its start when nothing narrower claims it
([[decisions/261105_the-error-record|the error record]]).
`Frame::abandon` supplies the undo shared by failure and the panic walk.
Destructuring parameters are binds at their own spans, so a mismatch points
at the parameter, at runtime as at type-check time.

## Values and environments

**Stored closures retain only what they mention.** `Node::new` computes
`Occ` once. `Closure::new` restricts ρ to a thunk node's occurrence set;
forcing restores that environment without bracketing shell state. A `cd`
in a forced body persists exactly as one in a lambda body does
([[design/scoping|scoping]]).

- `form(val, env, sig)` is side-effect-free. Constants form themselves,
  variables use `lookup`, variants form their payloads, and thunks close.
- Plain list, record and map literals are value closures too. Inspection
  distributes the saved environment one layer at a time. A name answered
  only by Σ requires eager formation, since literal inspection reads ρ alone.
- `Prim` computes on formed values. Collection assembly may cost O(data)
  and fail, but its operands have already run in surrounding binds.
- `Env` is an immutable finite map. Σ (`Signature`, natives and frozen
  prelude) is separate; `lookup` reads ρ, then Σ. Sinks, dynamic context,
  workers and leases belong to `Shell`, never to a closure.
- `Rec { group, index }` restricts ρ once to the group's shared occurrence
  set, binds each sibling to a thunk over the same group and environment,
  and enters the selected body. Recursive references re-enter through force;
  bodies are never substituted or rebuilt.

## Run boundaries

**The top level is a sequence of closed machines.** `run_phrases` runs
`Phrase::{Define, Run}` against the session environment. Each landed `Define`
is installed immediately; a later failure keeps earlier definitions.
A block instead nests `Bind` and `Seq`, whose lexical bindings do not escape.

- `evaluate` injects ⟨M, ρ⟩ over an empty stack; `apply` pushes arguments
  if any, and forces a closed value. Both return `Settled<Value>`.
- Higher-order natives enter nested machines on the host stack, bounded by
  `NESTED_MACHINE_LIMIT`. No native reads a caller's lexical environment.
- `Spawn` births a worker; `Pipeline` launches and joins a `PipeNode` in
  one shell rule. Ral stages run in threads over fresh stacks; external
  stages are processes. No frame crosses a thread or sealed-engine boundary
  ([[internals/pipeline-execution|pipeline execution]]).
- The step loop catches panics and abandons frames top-down. The framed run
  entry restores its checkpoint; cancellation is polled cooperatively.

See [[map/core/evaluator|evaluator]], [[map/core/ir|IR]],
[[map/core/shell-state|shell state]], [[design/cbpv|CBPV]].
