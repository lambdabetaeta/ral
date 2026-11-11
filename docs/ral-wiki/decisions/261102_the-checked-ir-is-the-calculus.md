---
status: active
generated_at_commit: 22b85799e5e435dff38ea1a2e7ec56d443d9f62b
generated_at_date: 2026-11-11
verified_at_commit: 22b85799e5e435dff38ea1a2e7ec56d443d9f62b
verified_at_date: 2026-11-11
anchors: [CompKind, Prim, Val, Focus, Terminal, Frame, Terminal::into_value, Solver::binding, Solver::argument, Readback::toplevel, captured_string]
---

> Amended 2026-11-11 by
> [[decisions/261111_a-guard-acquires|a-guard-acquires]]: the acquiring `guard`
> is **owned divergence D4** from `Extrinsic/`'s two-operand rule. The
> strong-model Agda follow-up owns syntax, substitution, typing, machine
> correspondence and affected examples; it must re-establish type safety
> and retire the divergence. The core partition below stands, but its guard
> correspondence claim is suspended until that work lands.

# The checked IR is the calculus

**The checked Rust IR and `Extrinsic/` share the CBPV partition.** Control is
syntax, operations on values are primitives, and shell interpretation is a
separate layer. The standing gaps below delimit correspondence; Box and
native terminals are not mechanised.

## Decision

- **Control:** `return`, `force`, lambda, application, pattern bind, sequence,
  recursion, `if`, `case`, `try`, `guard`, `capture`. Rust's CEK
  machine and Agda's substitution-based CK machine have corresponding rules
  apart from the standing gaps below;
  failure is a register in both. Application pushes one argument frame per
  argument, then forces. Lambda and native terminals consume arguments;
  residual functions box when none remain. Every `let` runs its RHS.
- **Primitives:** arithmetic, comparison, negation, rendering, concatenation,
  decoding, indexing, record update, list and map assembly. Operands are
  values; no primitive runs a computation or consults shell state. Each
  `ir::Prim` has an `Extrinsic.Prims.Op`, scheme and interpretation.
- **Shell:** `Exec`, `Pipeline`, `Redirect`, `Within`, `Grant`, `Spawn`,
  `Gather`, `Audit`, `Tilde` and protection membranes.
  Agda's sample `Builtin`s model resolved full-arity calls, not resolution.
  A native holds its entry and supplied arguments; forcing collects the rest
  in a native terminal. No builtin code enters IR. Boundary builtins remain
  η-expanded by inference over `Exec` with their per-use checked site.

**Every new IR form is classified into these layers.** A core control form
must be mirrored in Agda's syntax, typing and machine; a primitive in its
signature, interpretation and soundness proof. A shell form must state its
modelling boundary.

## Consequences

Hoisting makes collection assembly pure. Repeated computed map keys fail
only after all operands have run; this reverses the operand-ordering invariant
of ral `0cd41dbc`. Interpolation becomes render binds followed by concatenation.
The checker writes returned-thunk forces and binding captures explicitly. These
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
  `thunk (ƛⁿ prim (builtin b) x̄)`. Rust uses a native terminal and seals a
  partial builtin by name with its arguments, not as lambda code.
- **Boxing residual functions.** Rust boxes a λ that finds no argument,
  including at a binding or run boundary; the Agda Let-fun elaboration still
  suspends the whole RHS. This correspondence awaits its own follow-up.
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
- `core/src/typecheck/solve.rs`: `Solver::binding` and `Solver::argument`
  suspend the matches that decide binding modes and returned-thunk splits.
- `core/src/typecheck/readback.rs`: `Readback::toplevel` applies those decisions,
  snapshots sites and settles schemes after live contract holds and unit closure;
  `captured_string` builds capture/decode. See
  [[internals/type-inference|type inference]].
- `core/src/shell/evaluator/machine.rs`: `Focus`, `Terminal`, `Frame`.
- `dev/agda/Extrinsic/{Syntax/Term,Prims,Semantics/Machine}.agda`:
  corresponding syntax, primitives and CK rules.
- `dev/agda/README.md`: correspondence tables and standing gaps.
