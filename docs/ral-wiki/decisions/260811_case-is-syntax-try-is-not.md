---
status: active
---

# `case` is syntax; `try` is not

**A `case`'s arms are a syntactic list — `Vec<CaseArm>` in the AST and in the
IR — so every alternative is a computation the checker can see, and
exhaustiveness is decided statically, always.** `try` keeps its first-class
handler, and the asymmetry is principled rather than pending. The surface
spelling of `case` does not change: every example in the corpus compiles
untouched.

## One fact, found four times

`case` was an eliminator over a **value**: a tag-keyed record of thunks, forced
on match. But the checker must place a `Capture` coercion *inside each arm* at a
byte-side join, and it can only do that for an arm it can see. Four defects
found on 2026-08-11 are the same fact — an arm the checker cannot see gets no
coercion:

| where | the arm the checker could not see |
| --- | --- |
| `annotate_case_table`'s `Val::Thunk` guard | an arm written as a variable |
| a table that was not a `Val::Map` | `case $x $t` |
| a `ValMapEntry::Spread` in a literal table | `` [ `a: …, ...$rest ] `` |
| `collect_handler_arms`'s matching filter | an arm with no recorded result, so `ArmWalk::Wrap` was chosen by accident |

The witness typechecked:

```ral
let t = [ `some: { |p| echo b }, `none: { |p| echo z } ]
let q = case `some () $t            # printed b to the terminal; q was EMPTY
```

The payload escaped to the terminal and the capture returned nothing. The same
opacity cost the coverage proof its subject: exhaustiveness was checked only for
literal tables, so a computed one could miss a tag at run time. Making the arms
syntax makes each of these unrepresentable rather than diagnosed, and retires a
runtime failure mode outright.

## The line is the set of alternatives, not the alternatives

**The *set* of alternatives is syntax; each alternative's *body* is an ordinary
computation, however spelled.**

- The arms are written out at the `case`. `case $x $t` and a `...` spread arm
  are parse errors, as are a repeated tag, an empty arm list, a missing binder,
  and a two-parameter arm — each refused where no payload is yet in flight.
- An arm's body is any atom. `` `ok: $handler ``, `` `ok: $handlers[fallback] ``,
  and `` `ok: !{ pick } `` elaborate to the call the user could have written,
  `` `ok: { |p| $handler $p } ``, so the two spellings agree on type, route, and
  coercion by construction.
- The refactoring principle of [[design/types|types]] — a join is decided by the
  arms' types, never by how an arm was written — therefore survives intact for
  arm bodies, which is where it was ever exercised.

## Why `try` does not follow

- `case` owns a finite set of alternatives and must **prove that set
  exhaustive**. An opaque table defeats the *proof*, not merely the coercion:
  there is no label set to close the scrutinee's row against, so the judgment
  cannot be stated, let alone decided.
- `try` has exactly two outcomes, a value and a failure, and the form fixes
  them. Its branch *set* is never opaque; only the computation implementing the
  failure branch is, and a reusable recovery function is a natural first-class
  value whose single route its type states — which is why `try` records its
  handler's route (`typecheck/scope.rs`) and is correct today.
- This is pattern-match clauses being syntactic while the function passed to
  `map` stays first-class. Changing `try` would remove a useful abstraction and
  repair no invariant.

## Composable elimination algebras are excluded, permanently

A dispatch table assembled from reusable pieces, extended by spread, or selected
from configuration is **not wanted**. This is an exclusion, not a deferral.

- Such a table is a **totality claim with no proof behind it**. `case` exists to
  guarantee that every tag the scrutinee can carry has a computation to run.
- Assemble the alternatives elsewhere and exhaustiveness is checked against
  whichever pieces happened to be present at that call site. With open rows
  ([[design/row-types|row-types]]) the check then becomes a statement about a
  *value's history* rather than about the program.
- A table that is *nearly* total fails at run time, on the one input nobody
  assembled a piece for — the class of failure this language exists to refuse.

So there is no `Matcher` abstraction to be added later, and a record is not
readmitted as a proof of total elimination.

## What changes at run time

An arm was a lambda applied to the payload; it is an `if`-like branch: fresh
lexical scope for the pattern, body inline, tail position inherited. That is the
CBPV reading, and each consequence is taken deliberately.

- **The recorded status is inherited** at arm entry rather than reset by a
  lambda frame, and the status the arm leaves is the `case`'s own.
- **Every context mutation persists.** The lambda frame folded back
  `{last_status, cwd}` and discarded the rest of `mobile.context`; an arm now
  leaves all of it in place, as an `if` body does.
- **An unselected arm's hoisted effects no longer run.** `` `a: !{make-handler} ``
  forced before the `case` had chosen; it now runs only on selection.
- A tail call in an arm still escapes to the trampoline, because the arm
  inherits the `case`'s own tail position.

## In the code

- `CompKind::Case { scrutinee, arms: Vec<CaseArm> }`; a `CaseArm` is a tag, the
  `IrPattern` its payload binds, and an `ArmBody` ([[map/core/ir|ir]]).
  `ArmBody::{Inline, Applied}` records which spelling reached the branch, and
  serves diagnosis alone: a named handler that is not a function is faulted as
  an *arm*, in the vocabulary the user wrote, rather than as a command head.
- `annotate_case_table`, its `Val::Thunk` guard, the `case` path of
  `eta_expand_captured`, and `collect_handler_arms` are gone rather than fixed:
  an arm body is a `Comp`, so `annotate_join_arm` walks it exactly as it walks
  an `if` branch ([[internals/compilation-ladder|compilation-ladder]]).
- `infer_case` unifies the scrutinee's row with exactly the arms' labels,
  restating a row mismatch as exhaustiveness. An *open* scrutinee row still
  absorbs a label it has not been seen to construct: that is principal row
  inference, preserved on purpose.
- `infer_case` gains one companion, `infer_case_arm`. This is not the refactor
  [[decisions/260530_infer-case-stays-whole|infer-case-stays-whole]] forbids —
  that decision holds "unless its *behaviour* needs to change", and typing one
  arm is now a premise of the rule rather than a slice of one function.
- The diagnostics say *arm* everywhere, static and dynamic alike; the word
  *handler* is left to command handlers and `try`.

## The kernel already said this

The kernel ral is formalised against has Levy's eliminator, whose branches are
computations in the syntax:

```agda
case_of_,_ : Γ ⊢ᵛ A₁ + A₂ → Γ , A₁ ⊢ᶜ C → Γ , A₂ ⊢ᶜ C → Γ ⊢ᶜ C
```

The value eliminator was the surface's own invention, and the gap between the
two was the defect. `docs/SPEC.md` §8.3 gives the form, §17.1 the grammar,
§17.2 the core term, and §17.6 the typing rule.
