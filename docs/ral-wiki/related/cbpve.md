---
verified_at_commit: 8d868e18
verified_at_date: 2026-09-30
against: [design/types, design/cbpv, design/capture]
---

# CBPVE: grading call-by-push-value, and why ral does not

Dylan McDermott, *Grading Call-By-Push-Value, Explicitly and Implicitly*, FSCD
2025 (LIPIcs 337, `10.4230/LIPIcs.FSCD.2025.28`). CBPVE refines Levy's calculus
([[related/call-by-push-value|call-by-push-value]]) by annotating the returner
type with a grade: `F_e A` is the type of computations returning `A` with
behavioural grade `e`. It is the natural reference to read ral's computation
types against, and the reading is a negative one: **ral is not a graded CBPV.
Its returner carries no annotation at all: `F A` is Levy's, and a command is `F Unit`.**

## What ral's returner is, in the paper's own terms

CBPVE assumes grades form an **ordered monoid** `(E, ≤, 1, ·)`: `1` is the grade
of a computation with no effects, `d·e` grades running `d` then `e`, and `d ≤ e`
means `e` is more permissive. None of those pieces has a counterpart in ral's
`F A`:

- **No `1`.** A command is `F Unit`, and so is `cd`: writing is not in the type.
  What a computation may do to the world is the business of the grant that
  admits it, never of its type ([[design/types|types]]).
- **No `·`.** A sequence does not multiply its parts' annotations; it takes its
  tail's type, discarding every earlier one. `!{ echo a; return () }` is
  `F Unit` however loudly the head wrote.
- **No `≤`.** Arms that must agree are joined by unifying the values they
  return, not by a permissiveness order carried through the type system.

The one place a computation's writing matters to a program is `let`, and there
it is syntax: the checker wraps the command a `let` can see in the capture
coercion, `cap M to x. decode x`, before it infers any type
([[decisions/260930_capture-is-decided-by-syntax|capture-is-decided-by-syntax]]).
The paper itself supplies the sharpest way to see that nothing is graded. Its bind
rule

```
Γ ⊢ M : F_d A     Γ, x:A ⊢ N : C
────────────────────────────────
      Γ ⊢ M to x.N : ⟨⟨d⟩⟩C
```

applies a **grade action** `⟨⟨d⟩⟩` to the continuation's type, which is exactly
the move a grade must license: the operand's behaviour is not forgotten when the
tail is function-shaped. ral's bind performs no action: it hands back `N`'s type
untouched, and there is no `d` for it to act with.

## The grading that was, and why it went

An earlier ral did carry a Gifford-style pair `⟨reads-stdin, writes-stdout⟩` on
every computation type, which was a genuine instance of the paper's first
example (`E` the powerset of `Σ`, `·` the union, `≤` inclusion). It was removed
because nothing consumed it: the runtime routes descriptors from position and
never consulted an input mode, no builtin needed an output mode to arrange a
receiver, and equality over the pair rejected higher-order programs whose only
difference was whether they printed
([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).

The instructive part is *why* a may-write grade bought so little here. ral's
effects are operating-system effects on opaque children: it cannot know whether
an external binary reads its stdin, reads it partially, or ignores it. A grade
that cannot be inferred for the majority of a shell's computations degenerates
to a free variable, and a free variable that no rule reads is a deletion waiting
to happen. CBPVE's worked examples are languages whose operation set the type
system owns; a shell's is not.

## The coherence result, kept on file

The paper's central negative finding is that for a graded monadic semantics,
**coherence for implicitly graded terms is false in general**: different grade
derivations of the same ungraded term need not denote the same thing, so which
grading an inference engine picked would be semantically load bearing. ral is
out of that theorem's scope now, having no grades to infer. The sufficient
condition is worth recording anyway, because it is what any future ral effect
system should be checked against:

> **Definition 11.** An ordered monoid `E` has *left-cancellative upper bounds*
> if, whenever `d·e₁ ≤ d′ ≥ d·e₂`, there exists `e′` such that `e₁ ≤ e′ ≥ e₂`
> and `d·e′ ≤ d′`.
>
> **Theorem 12.** If `E` has left-cancellative upper bounds then coherence
> holds, in every graded model.

Any algebra whose multiplication *is* its join satisfies it: given `d ⊔ e₁ ⊑ d′`
and `d ⊔ e₂ ⊑ d′`, take `e′ = e₁ ⊔ e₂`; then `e₁, e₂ ⊑ e′` and
`d ⊔ e′ = (d ⊔ e₁) ⊔ (d ⊔ e₂) ⊑ d′` by idempotence and the least-upper-bound
property. The argument does not depend on the lattice being two-element, so it
survives a finer effect vocabulary — but it does depend on `·` remaining `⊔`. An
algebra that counted writes, or tracked order, would multiply differently and
the condition would need rechecking. **A pure may-use analysis is coherent for
free; a quantitative one is not.**

## Where ral would narrow CBPVE, if it graded

Three restrictions, each currently sound and each worth knowing before the
calculus grows:

- **No value subtyping.** CBPVE relates value types (`U C <: U D`, and
  componentwise at products and sums); ral has no subtyping relation at all.
- **Hence no `U C ≼ U D`** — a thunk holding a computation with slack cannot be
  re-typed. Nothing in ral needs it today.
- **Hence the arrow is invariant in its domain.** CBPVE's is *contravariant*:
  `A→C <: B→D` from `B <: A` and `C <: D`. ral's invariance is not a separate
  choice; it is the first restriction seen at the arrow.

And where CBPVE has a general `coerce_D M` over the whole of `<:`, ral has one
coercion, `capture`, a term the checker writes at a `let` ([[design/types|types]]),
not a step of a subtyping derivation.

## What ral could borrow

The graded-monad semantics of §5 interprets `F_e A` as `e∗ F T⟦A⟧` over algebras
of a graded monad. Nothing in ral needs it while `F` is ungraded, but it is the
shape a denotational model would take the day the shell wants a real effect
discipline over its syscall signature
([[design/syscalls-are-effects|syscalls-are-effects]]) rather than a capture
coercion. The `⊤⊤`-lifting logical relation of §6.1 — varying-arity, indexed by
contexts — is the technique a relational model of ral would want either way, and
is the reason a substitution kit and a well-formedness predicate are the *first*
things such a development needs.

See also [[related/call-by-push-value|call-by-push-value]],
[[design/types|types]], [[design/capture|capture]].
