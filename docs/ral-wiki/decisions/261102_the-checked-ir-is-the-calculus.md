---
status: active
generated_at_commit: 9290eb03
generated_at_date: 2026-11-13
verified_at_commit: 9290eb03
verified_at_date: 2026-11-13
anchors: [CompKind, CompKind::Builtin, CompKind::Use, Prim, Val, ValClos, BuiltinEntry::eta, Stack::settle, Frame, Solver::binding, Solver::argument, Readback::toplevel, captured_string]
---

# The checked IR is the calculus

**The checked Rust IR has the CBPV partition: control is syntax, operations
on values are primitives, and shell interpretation is a separate layer.**
Every new IR form is classified into one of the three.

## Decision

- **Control:** `Return`, `Force`, `Lam`, `App`, `Bind`, `Seq`, `Rec`, `If`,
  `Case`, `Try`, `Guard`, `Capture`. Failure is the machine's `Err(Break)`
  state, not a computation. `App` pushes one argument frame per argument and
  forces; a λ consumes arguments, and a residual function is boxed when none
  remain. Every `let` runs its RHS.
- **Primitives:** `Binary`, `Negate`, `Not`, `Render`, `Concat`, `Decode`,
  `Index`, `Update`, `List`, `Map`. Operands are values; no primitive runs a
  computation, resolves a name or consults shell state. It may fail.
- **Shell:** `Exec`, `Pipeline`, `Redirect`, `Within`, `Grant`, `Spawn`,
  `Gather`, `Audit`, `Tilde`, `Use`. `Use(Arc<Module>)` is a checked module
  linked by elaboration from one written path; the checker instantiates its
  decided public schemes, the evaluator runs it under Σ alone and returns its
  public bindings as a record. Source reads, cycle and depth checks belong to
  linking.
- **Runtime-only δ:** `CompKind::Builtin` is a host-operation form, distinct
  from the value-only `Prim`. The elaborator writes none; a builtin forces to
  a λ-chain ending in δ, which reads its entry and arguments from ρ, and
  `ValClos::block` keeps residuals out of rechecking
  ([[261112_runtime-values-are-value-closures|runtime values are value closures]]).
  Boundary builtins stay inference-expanded over `Exec`, keeping their per-use
  checked site.

The machine state is ⟨M, ρ⟩ or `Err(e)`; `Stack::settle` passes values down
without a register ([[map/core/evaluator|evaluator]],
[[internals/evaluator-machine|the machine narrative]]).

## Consequences

Elaboration and the checker write what would otherwise be evaluator
mechanism: [[invariants/ir-pure-cbpv|the IR invariant]] states the resulting
rules. Hoisting makes collection assembly pure: a repeated computed map key
fails only after all operands have run. Interpolation is render binds then one
concatenation. The checker writes returned-thunk forces and binding captures
(`Capture(M) to d. Prim::Decode(d)`) explicitly
([[260811_a-coercion-is-syntax|a coercion is syntax]]).

- A bare tag carries `()`: `` `t `` and `` `t () `` are one value, in grant
  decoding too.
- Only a written sequence or `let` adds the abandoned-steps hint; synthetic
  binds add none.
- Destructuring parameters bind after the whole lambda chain; a later plain
  name also gets a body bind when an earlier destructuring parameter binds it.
- A spread into a written value head or a lexically bound head is an
  elaboration refusal; builtin and session-binding heads refuse in the
  checker (T0056).
- A runtime destructuring-parameter mismatch points at the parameter, not the
  call site.
- Host-file and nested-table options are checked key by written key during
  generation (SPEC §15.3). Readback freezes sites and settles schemes after
  `Solver::close` succeeds (SPEC §17.5).

## Runtime boundaries

- **Higher-order builtins** (`map`, `each`, …) run their function arguments in
  a nested machine through `machine::apply`, a host function, not a machine
  rule.
- **Execution outside the machine:** `Pipeline` stages use nested `evaluate`,
  `Spawn` bodies use `machine::apply` on a worker, session hooks use
  `evaluator::apply`, and `Gather` joins its children.
- **Errors and numbers:** errors are data records; numbers are checked `i64`
  and finite `f64`.
- `guard` acquires a resource and passes it to the body and the release; the
  release also observes the body's ending
  ([[261111_a-guard-acquires|a guard acquires]]).

## Where

- `core/src/ir.rs`: `CompKind`, `CompKind::Builtin`, `CompKind::Use`, `Prim`,
  `Val`; `core/src/typecheck/env.rs`: `Inferring` excludes δ.
- `core/src/typecheck/solve.rs`: `Solver::binding` and `Solver::argument`
  suspend the matches that decide binding modes and returned-thunk splits.
- `core/src/typecheck/readback.rs`: `Readback::toplevel` applies those
  decisions and settles schemes after successful unit closure;
  `captured_string` builds capture/decode
  ([[internals/type-inference|type inference]]).
- `core/src/shell/evaluator/machine.rs`: `Stack::settle`, `Frame`,
  `CompClosure`, `Next`; `core/src/ir/value.rs`: `ValClos`;
  `core/src/builtin.rs`: `BuiltinEntry::eta`.
