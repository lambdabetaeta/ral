---
status: proposed
---

# `!` eliminates a block, not a function

The surface force operator should eliminate a value-producing thunk; forcing a function is a category error the checker should reject where the function is supplied.

## The decision

**The surface `!` requires its operand to be a value-producing thunk — a *block*, `U(F α)` — not a function-thunk `U(A → B)`.** A block-forcing parameter then carries the type "nullary block", so a function-bodied argument is rejected at the call site instead of silently returned.

- `CompKind::Force(Val)` types its operand against `Thunk(cty)` for a *fresh* `cty` (`infer.rs` Force rule), admitting any computation type, `Fun` included. So `!$body` with a polymorphic `body` accepts a function-thunk, and at runtime `step_force` returns it unchanged — a `Value::Lambda` is a function still awaiting arguments, so nothing runs.
- The hole is narrow but real. A *value-demanding* context already rejects a function-shaped force: `try { !$f }` is a static `[T0011]` (the try body must be value-shaped). A force whose result is merely *displayed* or *returned* is not: `let w = { |name body| !$body }; w 'x' { |s| $s }` exits 0 and runs nothing. The value-level [[design/cbpv|Block/Lambda]] split has no type-level mirror at the force site.
- The remedy is a *missing type*, in the [[decisions/260614_structural-bug-prevention|structural-bug-prevention]] spirit: give `!` the rule `operand : U(F α)`. Then `with-secret`'s `body` parameter is inferred as a nullary block, and `with-secret 'x' { |s| … }` is a static error at the argument — the bad state is unconstructable, not guarded at runtime.

## Why not the runtime arm

The obvious one-line fix — make `step_force` error on a `Value::Lambda` — is unsound.

- The elaborator lowers every bound-name call head to `App(Force(Variable n), args)` (`elaborator.rs`, the `Head::Bare`/`is_bound` branch). `step_force` returning a `Value::Lambda` unchanged is therefore *how named functions are applied*: the head is forced to its function, then the trampoline applies the arguments. Erroring in that arm breaks all named-function calls.
- The surface `!` and this synthesised call-head force are the **same** `CompKind::Force`. The call head must force a function-thunk; the surface `!` must not. The rule can only diverge once the two sites are told apart.

## The open question — telling the two force sites apart

This is the cost, and the part still to settle, weighed against [[invariants/ir-pure-cbpv|ir-pure-cbpv]] (the IR carries no surface distinction):

- **Force-free call heads** — application eliminates a thunk-bound head intrinsically, so `CompKind::Force` is left to mean the surface `!` alone and its rule is uniformly strict. Most CBPV-faithful (*force* is the user's operator; *calling* is application) and it needs no flag, but it is the heaviest change: the elaborator's head lowering and the evaluator's `App` head path both move.
- **An origin flag on `Force`** — the elaborator marks user-`!` against the synthesised head; the checker branches on it. Surgical, but it leaves a faint surface trace in the IR, in tension with ir-pure-cbpv.
- **A second eliminator** — a distinct IR node for the call head. The most explicit, the largest IR surface.

The Force-free-call-head shape is the principled target — it removes the ambiguity rather than annotating around it — and is preferred unless the `App` head path proves too costly to relocate, in which case the origin flag is the surgical interim.

## Where

- **`core/src/ir.rs`** — `CompKind::Force(Val)`, the single eliminator; `Val::Thunk`, created by both `{ … }` blocks and lambdas.
- **`core/src/elaborator.rs`** — the `Head::Bare` bound branch lowers a call head to `Force(Variable)` before `apply_head`.
- **`core/src/typecheck/infer.rs`** — the `CompKind::Force` rule unifies the operand with `Thunk(fresh cty)`; the strict rule unifies against `Thunk(F α)`.
- **`core/src/evaluator/comp.rs`** — `step_force`: runs a `Block`, returns a `Lambda` unchanged.
- **`core/src/types/value.rs`** — `Value::Block` vs `Value::Lambda`, the value-level split this decision mirrors into the types.

## The hard rule

`!{ … }` forces and runs a block. `!e` where `e` is, or instantiates to, a function is a static error — reported at the expression that supplies the function, never a silent no-op. Forcing a thunk-bound head to its function stays the business of application, not of the surface `!`.

See also [[design/cbpv|cbpv]], [[invariants/ir-pure-cbpv|ir-pure-cbpv]], [[decisions/260614_structural-bug-prevention|structural-bug-prevention]], [[map/core/typecheck|typecheck]], [[map/core/elaboration|elaboration]], and [[map/core/evaluator|evaluator]].
