---
status: active
generated_at_commit: f06a5056
---

# Unitarity lives in the term rules

**The assignment `Δ` is withdrawn.** An absent field is absent, full stop: it
carries no type, and retiring a field imposes no equation on the payload that
dies with it.

> `Δ` made the algebra unitary so the unifier is order-independent *on its
> own*. Under this design, order-independence is a theorem about the term rules
> (confinement) plus the one-optional-type door.

## Context

[[decisions/260921_a-field-is-a-flag-and-a-type|a-field-is-a-flag-and-a-type]]
bought unitarity with an assignment. `Δ` mapped a label to a type variable
`δ_l`, minted on the first retirement at that label and living on `Unifier`;
retiring a field unified the dying payload with `δ_l`. The deciding equation

```
Record(l: θ·α ; Empty)  ≐  Record(l: θ'·String ; Empty)
```

then had one solution rather than two, because the solution that kills the
field said something — `String ~ δ_l` — instead of throwing `α` away.

That is a real theorem about the *algebra*, and it is more than was needed.
What a typechecker owes is that **the programs a user can write** get one
verdict whichever order their statements are in. The algebra is the wrong
quantifier: it ranges over every row the datatype can spell, including rows no
rule mints.

## Decision

`Δ` goes. `Field::Absent` carries nothing and means nothing further; the field
rule unifies the flags and then the payloads *when both sides have one*, and
drops the equation otherwise.

What replaces the algebraic argument is a **confinement invariant** on the term
rules:

> Every variable occurring in the payload of a `Field::Var(θ, τ)` slot at label
> `l` occurs nowhere but in payloads of slots whose flag is in `θ`'s union-find
> class.

`Field::Var` is minted in exactly two places — the put rule in
`infer_record_val`, a fresh flag over a *fresh* payload, and `scope::occurrence`
in `typecheck/scope.rs`, a fresh flag over a ground `At(T)`, a fresh variable
for `Decoded`/`Refused`, or a fresh shape for `Shaped`. No builtin scheme
carries one, and the only operation that touches a `Var` payload is
`unify_field`. So the equation `Δ` used to add could bind only variables that
nothing else names — dead, therefore unobservable — and could *fail* only when
two declared tables disagree at one label.

That last case is exactly what the door in `typecheck/contract.rs` already
forbids, so nothing is lost by deleting the equation and the store behind it.

**This makes the door load-bearing.** Under `Δ` the one-optional-type condition
was a nicety that kept an assignment variable from being asked for two types;
now it is half the reason a verdict does not depend on statement order.
Confinement must be re-established for **every new site that introduces a
variable flag** — a third minting site, or a builtin scheme carrying a `Var`
payload, breaks the argument and not merely a comment.

The door is also sharpened as part of the withdrawal: it compares
**unifiability** rather than equality, minting both declarations in a scratch
unifier, which subsumes the old ground-type equality test and stops exempting
`Holds::Shaped`. A shape at a label another table holds at a ground type is the
same order-dependence as two ground types.

## Consequences

- **Erasure now really erases.** A retired payload is dropped rather than
  re-homed, so `generalize`'s residual cache is sound for *all five* sorts
  again: `residuals_are_live_roots` recovers the `ty_fv` clause `Δ` forced it to
  drop, and its "holds by construction" reason with it.
- **Retiring a field costs no depth.** The retirement used to be the type
  equation `τ ~ δ_l`, which fingerprinted `τ` and could raise `TypeTooDeep` for
  a payload nothing would ever read. It no longer walks the payload at all.
- **Two latent defects go with the equation.** The flag was bound *before* the
  payload equation ran, so a failed payload left the flag `Absent` with the
  payload never united; and the claim that `δ_l` was always on the left of the
  union — so a payload some scheme still named never stopped being canonical —
  was false from the second retirement at a label onward.
- **The deciding equation has two solutions again, and that is fine.** They
  differ only in the binding of a dead variable. The unifier test asserts
  *observables* — both flags, both rows as applied — rather than a store
  binding, and a new inference-level test writes one wrapper's `within $o` and
  `grant $o` in both statement orders and demands one verdict.
- **No reachable program changes verdict.** Every `.ral` file in the tree gives
  byte-identical `--check` output across the withdrawal.
