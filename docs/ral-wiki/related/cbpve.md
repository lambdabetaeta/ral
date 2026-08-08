---
verified_at_commit: e4d7bd27
verified_at_date: 2026-08-08
against: [design/types, design/cbpv, design/capture]
---

# CBPVE: grading call-by-push-value, explicitly and implicitly

Dylan McDermott, *Grading Call-By-Push-Value, Explicitly and Implicitly*, FSCD
2025 (LIPIcs 337, `10.4230/LIPIcs.FSCD.2025.28`). CBPVE refines Levy's calculus
([[related/call-by-push-value|call-by-push-value]]) by annotating the returner
type with a grade: `F_e A` is the type of computations returning `A` with
behavioural grade `e`. It is the published account of what ral's
[[design/types|byte modes]] are doing, and ral turns out to be one of its
worked examples.

## ral is a named instance, not an analogy

CBPVE assumes grades form an **ordered monoid** `(E, ≤, 1, ·)`: a monoid with a
monotone multiplication, where `1` is the grade of a computation with no
effects, `d·e` is the grade of running `d` then `e`, and `d ≤ e` means `e` is
more permissive. The paper's first instance is ral's:

> take `E` to be the powerset of `Σ`, ordered by inclusion, and with union for
> the multiplication … a Gifford-style type-and-effect system that tracks which
> operations a computation may use as it runs.

Read off against ral: `Σ = {reads-stdin, writes-stdout}`, so `E` is a
four-element lattice, `·` is `⊔`, `≤` is `⊑`, and `1` is `⟨∅,∅⟩`. `return V :
F₁ A` is exactly ral's `return : ⟨∅,∅,∅⟩ A`. That grades *bound* rather than
oblige — a `⟨i,o,r⟩` licenses reading and writing without requiring them — is
the same "may use" reading the paper's `≤` carries.

## The bind rule is the action, and `r` is not a grade

CBPVE's bind is

```
Γ ⊢ M : F_d A     Γ, x:A ⊢ N : C
────────────────────────────────
      Γ ⊢ M to x.N : ⟨⟨d⟩⟩C
```

where `⟨⟨d⟩⟩C` is the **grade action** on computation types, defined so that a
grade applied to an arrow distributes into its codomain. That action is the
reason a bind may not drop its operand's grade when the continuation is
function-shaped, and it is why ral's checker demanding an operand be
channel-silent is the conservative repair of the same leak.

Note what `d` ranges over: **one** grade, applied to the whole tail. In
`⟨i,o,r⟩ A` the grade is the pair `⟨i,o⟩`. The result mode `r` is not part of
it — it is a tag on the returner type naming *which conduit carries the
payload*, the return value or the byte channel. So the action joins `i` and `o`
and keeps the tail's `r`, which is what ral's one `Bind` arm computes.

## Coherence: the paper's negative result does not bite ral

This is the substantive reading, and it is a claim about ral worth stating
outright.

CBPVE is the **explicit** calculus: grades are in the syntax, and coercions are
a term former `coerce_D M` licensed by a subtyping relation `C <: D`. ral is the
**implicit** case — the surface syntax carries no grades and the mode solver
infers them. The paper studies exactly that reading, `Γ ⊢i M : C`, defined as
"some CBPVE term erases to `M`", and shows that to interpret such a term one
needs a **coherence** result: different grade derivations for the same ungraded
term must denote the same thing. Its central negative finding is that for a
graded monadic semantics **coherence is false in general**. Were that to apply
to ral, *which* grades the solver picked would be semantically load bearing, not
merely a matter of precision.

The paper then gives a sufficient condition.

> **Definition 11.** An ordered monoid `E` has *left-cancellative upper bounds*
> if, whenever `d·e₁ ≤ d′ ≥ d·e₂`, there exists `e′` such that `e₁ ≤ e′ ≥ e₂`
> and `d·e′ ≤ d′`.
>
> **Theorem 12.** If `E` has left-cancellative upper bounds then coherence
> holds, in every graded model.

ral satisfies it, and for a structural reason rather than by luck. Suppose
`d ⊔ e₁ ⊑ d′` and `d ⊔ e₂ ⊑ d′`; take `e′ = e₁ ⊔ e₂`. Then `e₁ ⊑ e′` and
`e₂ ⊑ e′` immediately, and `d ⊔ e′ = (d ⊔ e₁) ⊔ (d ⊔ e₂) ⊑ d′` by idempotence
of the join and the least-upper-bound property. So:

**In any grade algebra whose multiplication *is* its join, Definition 11 holds.**

ral's does, because its effect discipline is a pure may-use analysis with no
notion of how many times an operation ran. Hence ral's implicit grade
assignment is coherent in every graded monad model, and the solver is free to
pick any admissible grading.

Two things worth noting about the shape of that argument. It does **not** depend
on `Mode` having two inhabitants, so it survives extending the lattice with a
third channel or finer grades — unlike encodings that exploit the two-element
case. But it does depend on `·` remaining `⊔`: a grade algebra that counted
writes, or tracked order, would multiply differently and the condition would
have to be rechecked.

## Where ral narrows CBPVE

Three restrictions, each currently sound and each worth knowing before the
calculus grows:

- **No value subtyping.** CBPVE relates value types (`U C <: U D`, and
  componentwise at products and sums); ral's bound relates computation types
  only.
- **Hence no `U C ≼ U D`** — a thunk holding a computation with grade slack
  cannot be re-typed. Nothing in ral needs it today.
- **Hence the arrow is invariant in its domain.** CBPVE's is *contravariant*:
  `A→C <: B→D` from `B <: A` and `C <: D`. ral's invariance is not a separate
  choice; it is the first restriction seen at the arrow.

And where CBPVE has a general `coerce_D M` over the whole of `<:`, ral has
[[design/types|one subsumption instance]] — a computation with no payload of its
own may be read as one whose payload is its byte channel — realised as the
single coercion `capture` moves the other way. A one-instance fragment of a
published subtyping relation is a deliberate narrowing, not an omission.

## What ral could borrow

The graded-monad semantics of §5 is the obvious next thing: it interprets `F_e
A` as `e∗ F T⟦A⟧` over algebras of a graded monad, which is the shape a
denotational model of ral's byte channel would take, and Theorem 12 says such a
model is available. The `⊤⊤`-lifting logical relation of §6.1 — varying-arity,
indexed by contexts — is also the technique a relational model of ral would
want, and is the reason a substitution kit and a well-formedness predicate are
the *first* things such a development needs.

See also [[related/call-by-push-value|call-by-push-value]],
[[design/types|types]], [[design/capture|capture]].
