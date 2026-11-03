---
status: active
generated_at_commit: b1ba0713
generated_at_date: 2026-11-03
verified_at_commit: b1ba0713
verified_at_date: 2026-11-03
anchors: [CompKind, Prim, Val, Focus, Terminal, Frame, native_code, captured_string, Inferencer::split, rhs_bound_ty, freeze_toplevel]
---

# The checked IR is the calculus

**The checked Rust IR and `Extrinsic/` describe one CBPV core.** Control is
syntax, operations on values are primitives, and shell interpretation is a
separate layer.

## Decision

- **Control:** `return`, `force`, lambda, application, pattern bind, sequence,
  recursion, `if`, `case`, `try`, `guard`, `capture`, `upcast`. Rust's CEK
  machine and Agda's substitution-based CK machine have corresponding rules;
  failure is a register in both. Application pushes one argument frame per
  argument, then forces; only a lambda terminal consumes an argument.
- **Primitives:** arithmetic, comparison, negation, rendering, concatenation,
  decoding, indexing, record update, list and map assembly. Operands are
  values; no primitive runs a computation or consults shell state. Each
  `ir::Prim` has an `Extrinsic.Prims.Op`, scheme and interpretation.
- **Shell:** `Exec`, `Pipeline`, `Redirect`, `Within`, `Grant`, `Spawn`,
  `Gather`, `Audit`, `Tilde`, protection membranes, and runtime-only `Native`.
  Agda's sample `Builtin`s model resolved full-arity calls, not resolution.
  Forcing a native builds its lambda chain from the entry in hand; partial
  application is a closure, with received arguments in its environment.

**Every new IR form is classified into these layers.** A core control form
must be mirrored in Agda's syntax, typing and machine; a primitive in its
signature, interpretation and soundness proof. A shell form must state its
modelling boundary.

## Consequences

Hoisting makes collection assembly pure. Repeated computed map keys fail
only after all operands have run; this reverses the operand-ordering invariant
of ral `0cd41dbc`. Interpolation becomes render binds followed by concatenation.
The checker writes returned-thunk forces and upcasts explicitly. These
translations remove evaluator mechanisms that would otherwise sit outside
the calculus. [[invariants/ir-pure-cbpv|The IR invariant]] states the resulting
partition and elaboration rules.

- A bare tag carries `()`: `` `t `` and `` `t () `` are one value, including
  in grant decoding.
- Only a written sequence or `let` adds the abandoned-steps hint; synthetic
  binds add none.
- Destructuring parameters bind after the whole lambda chain; a later plain
  name also uses a body bind when an earlier destructuring parameter binds it.
- A spread into a written value head or lexically bound head is an elaboration
  refusal (P0001, exit 2); builtin and session-binding heads retain T0056.
- A runtime destructuring-parameter mismatch points at the parameter, not the
  call site.

This amends the native representation of
[[260801_a-name-is-a-value-or-it-is-handled|a name is a value or it is handled]],
the coercion representation of [[260811_a-coercion-is-syntax|a coercion is syntax]],
and the application and sequencing rules of
[[260826_the-evaluator-steps-closures|the evaluator steps closures]].

## Standing gaps

- **Substitution vs environments.** Agda's `Val` represents both `ir::Val`
  and runtime `Value`, hence `bytes`; a native is
  `thunk (ƛⁿ prim (builtin b) x̄)`. Rust builds the lambda chain when forced.
- **Upcast:** Rust's tail step fuses Agda's frame push and identity pop.
- **Higher-order natives** (`map`, `each`, …) run their function arguments
  through a nested machine (`machine::apply`), a host function rather than
  a machine rule. Not modelled.
- **Errors and numbers:** `Err` is `String` in `Extrinsic.Prims`, a data
  record in Rust; Agda uses ℤ/`Float`, Rust checked `i64`/finite `f64`.
  `Intrinsic.Arithmetic` has the checked arithmetic model.
- **Shell-layer execution outside the machine:** `Pipeline` stages use
  nested `evaluate`, `Spawn` bodies use `machine::apply` on a worker,
  session hooks use `evaluator::apply`, and `Gather` joins its children.
  These nested-machine, thread and join behaviours are not modelled.

## Where

- `core/src/ir.rs`: `CompKind`, `Prim`, `Val`.
- `core/src/typecheck/infer.rs`: `infer_let`, `captured_string` and
  `Inferencer::split` construct binding coercions and returned-thunk splits;
  `grade.rs::rhs_bound_ty` decides binding capture and Let-fun.
- `core/src/typecheck/freeze.rs`: `freeze_toplevel` snapshots sites and settles
  schemes after successful inference and ascription.
- `core/src/shell/evaluator/machine.rs`: `Focus`, `Terminal`, `Frame`.
- `dev/agda/Extrinsic/{Syntax/Term,Prims,Semantics/Machine}.agda`:
  corresponding syntax, primitives and CK rules.
- `dev/agda/README.md`: correspondence tables and standing gaps.
