---
status: active
generated_at_commit: c0905f5e
generated_at_date: 2026-11-12
verified_at_commit: c0905f5e
verified_at_date: 2026-11-12
anchors: [CompKind, CompKind::Builtin, Prim, Val, ValClos, BuiltinEntry::eta, Machine::settle, Frame, Solver::binding, Solver::argument, Readback::toplevel, captured_string]
amended_by: "[[261112_runtime-values-are-value-closures|runtime values are value closures]]"
---

> **Amended 2026-11-12 — One value syntax.**
> [[261112_runtime-values-are-value-closures|Runtime values are value closures]]
> adds runtime constants `Val::Bytes`, `Val::Builtin` and `Val::Handle`, and
> runtime-only `CompKind::Builtin` (δ). A builtin forces to a λ-chain ending
> in δ, which reads its entry and arguments from ρ; partial application is
> an ordinary boxed λ and crosses the wire as a thunk. Exactly saturated
> runtime calls may invoke the entry directly. δ is a host-operation form,
> distinct from value-only `Prim`; the elaborator writes none, and
> `ValClos::block` keeps η residuals out of rechecking. The state is ⟨M, ρ⟩
> or `Err(e)`; values pass down through
> `Machine::settle` without a persistent register. This withdraws the native
> terminal, separate runtime value syntax and “no builtin code enters IR”
> representation claims below, not the control/primitive/shell partition.

> **Amended 2026-11-12 — `Use` is shell IR.** The shell layer additionally
> contains `Use(Arc<Module>)`: a checked `Toplevel` and its `SourceOrigin`,
> linked by elaboration from one written path at check time. Its checker
> rule instantiates the module's decided public schemes; its evaluator rule
> runs that tree under Σ and returns the public bindings as a record.
> A module is checked against Σ alone, with no importer or session bindings.
> `Use` is total in every phase and needs no new phase-associated type.
> Its source reads, cycle and depth checks belong to linking, not evaluation.
> The control/primitive partition below is unchanged.

> **Amended 2026-11-11 — Written host options.** Host-file and nested-table
> fields are checked key by written key during generation (SPEC §15.3).
> Readback freezes sites and settles schemes after `Solver::close` succeeds,
> without a separate live return-contract phase (SPEC §17.5); this supersedes
> that phase's mention in `Where`. The calculus partition below is unchanged.

> Amended 2026-11-11 by
> [[decisions/261111_a-guard-acquires|a-guard-acquires]]: `guard` acquires a
> resource and passes it to the body and release; release also observes the
> body's ending. The core partition below stands.

# The checked IR is the calculus

**The checked Rust IR has the CBPV partition.** Control is syntax, operations
on values are primitives, and shell interpretation is a separate layer.

## Decision

- **Control:** `return`, `force`, lambda, application, pattern bind, sequence,
  recursion, `if`, `case`, `try`, `guard`, `capture`. Failure is a register
  in the CEK machine. Application pushes one argument frame per
  argument, then forces. Lambda and native terminals consume arguments;
  residual functions box when none remain. Every `let` runs its RHS.
- **Primitives:** arithmetic, comparison, negation, rendering, concatenation,
  decoding, indexing, record update, list and map assembly. Operands are
  values; no primitive runs a computation or consults shell state.
- **Shell:** `Exec`, `Pipeline`, `Redirect`, `Within`, `Grant`, `Spawn`,
  `Gather`, `Audit`, `Tilde` and protection membranes.
  A native holds its entry and supplied arguments; forcing collects the rest
  in a native terminal. No builtin code enters IR. Boundary builtins remain
  η-expanded by inference over `Exec` with their per-use checked site.

**Every new IR form is classified into these layers.**

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

This amends the coercion representation of [[260811_a-coercion-is-syntax|a coercion is syntax]],
and the application and sequencing rules of
[[260826_the-evaluator-steps-closures|the evaluator steps closures]].

## Runtime boundaries

- **Higher-order natives** (`map`, `each`, …) run their function arguments
  through a nested machine (`machine::apply`), a host function rather than
  a machine rule.
- **Errors and numbers:** errors are data records; numbers use checked
  `i64` and finite `f64`.
- **Shell-layer execution outside the machine:** `Pipeline` stages use
  nested `evaluate`, `Spawn` bodies use `machine::apply` on a worker,
  session hooks use `evaluator::apply`, and `Gather` joins its children.

## Where

- `core/src/ir.rs`: `CompKind`, `CompKind::Builtin`, `Prim`, `Val`;
  `core/src/typecheck/env.rs`: `Inferring` excludes δ.
- `core/src/typecheck/solve.rs`: `Solver::binding` and `Solver::argument`
  suspend the matches that decide binding modes and returned-thunk splits.
- `core/src/typecheck/readback.rs`: `Readback::toplevel` applies those decisions,
  snapshots sites and settles schemes after successful unit closure;
  `captured_string` builds capture/decode. See
  [[internals/type-inference|type inference]].
- `core/src/shell/evaluator/machine.rs`: `Machine::settle`, `Frame`, `State`,
  `Next`; `core/src/shell/runtime/value.rs`: `ValClos`;
  `core/src/builtin.rs`: `BuiltinEntry::eta`.
