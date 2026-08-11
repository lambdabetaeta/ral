---
verified_at_commit: 95449d4
verified_at_date: 2026-08-10
against: [design/cbpv, design/types, design/pipelines, internals/evaluator-machine]
---

# Call-by-push-value — the substrate, taken as surface design

Levy, *Call-by-Push-Value: A Subsuming Paradigm*, TLCA 1999; the 2003 book
*Call-by-Push-Value: A Functional/Imperative Synthesis*.

**ral is CBPV in the wild: Levy's calculus is not just its IR but its surface
design.** "A value *is*, a computation *does*" is realised as a user-facing
discipline — data never executes, and only a forced command in head position
touches the world ([[design/cbpv|cbpv]],
[[invariants/ir-pure-cbpv|ir-pure-cbpv]]).

## What ral takes whole

- **The two sorts, surfaced as the two sigils.** Values vs computations is
  Levy's split; ral exposes it as `$name` (dereference, never forces) against
  head position (force). The thunk `{M}` is `U`; a command returning `A` is an
  `F`; blocks are literally `U(A → B)` — the CBV image of functions, written
  honestly.
- **Sequencing is `to`, and it earns its keep in inference.** ral's `let` over
  a command is Levy's `M to x. N`. Generalisation at `Bind` needs **no value
  restriction** precisely because CBPV sequences the effect *before* binding —
  the thing generalised is always a value whose effect has already happened
  ([[design/types|types]]). The substrate does real type-theoretic work.
- **The subsumption is live, not historical.** Eager application is the
  call-by-value image; passing a `{M}` thunk recovers a call-by-name call site
  term-by-term. Both disciplines are expressible and neither is baked in —
  which is Levy's theorem used as a language-design budget.
- **The machine is the CK reading.** ral's evaluator is a trampolined CBPV
  machine: a tail call is emitted as `Control::Tail` and absorbed by a loop,
  never a host frame ([[internals/evaluator-machine|evaluator-machine]]). That
  is Levy's jumping intuition — *calling a procedure is a jump, and returning
  is also a jump* — with the stack discipline enforced by `pub(crate)`
  visibility rather than by a calculus of stacks.

## Divergences (extensions, mostly)

- **`F` carries one annotation, and it is not a grade.** ral's returner is
  `F[ρ] A`, where `ρ ∈ {Value, Bytes}` says which of a computation's two
  products a *value boundary* observes — the returned `A`, or the stdout it
  wrote ([[design/types|types]]). It bounds no effect, licenses nothing, and
  does not multiply along a bind: `M to x. N` simply takes `N`'s route. The
  formation rule `ρ = Bytes ⇒ A = Unit` is the whole of its theory. Strip the
  annotation and what is left is Levy's calculus unchanged
  ([[decisions/260809_pipes-are-positional-byte-wires|pipes-are-positional-byte-wires]]).
- **The pipe is a new computation combinator, and it is not a typing fact.**
  CBPV composes computations by sequencing and application only. ral adds `|`,
  whose static rule says just that both sides are computations — `Γ ⊢ M : F[ρ] A`
  and `Γ ⊢ N : F[σ] B` give `Γ ⊢ M | N : F[σ] B`. What the combinator *does* is
  operational: it connects `M`'s stdout to `N`'s stdin with an
  operating-system pipe, discards `M`'s returned value, and runs both in one
  process group ([[design/pipelines|pipelines]]). It is not a CBPV connective;
  it is exactly where ral is a shell rather than a λ-calculus, and the honest
  reading is that the shell's one composition operator lives outside the
  calculus rather than being encoded into it.
- **No computation products.** ral's computation types are `F[ρ] A` and
  `A → C`, full stop; Levy's `Πᵢ Bᵢ` is absent. Where it would be used, a
  record of thunks — a value product of `U`s — serves.
- **The effect interface is fixed.** Levy's calculus is effect-agnostic; ral
  pins the operation signature at the external-command boundary
  ([[design/syscalls-are-effects|syscalls-are-effects]]), the Plotkin–Power
  layer over CBPV
  ([[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]]).

## What ral could borrow

- **The proof vocabulary, when SPEC §4 is formalised.** Levy's stack machine
  and the adjunction models with stacks are the off-the-shelf framework in
  which `Control::Tail` / `Settled` become statements about stacks, and the
  trampoline's correctness a simulation result — the jumping-semantics paper
  is the bridge.
- **The βη-theory for the pure fragment.** [[design/cbpv|cbpv]]'s "equational
  reasoning in the pure fragment" can cite CBPV's equational theory verbatim
  rather than re-deriving it.

Cite: Levy, *Call-by-Push-Value: A Subsuming Paradigm* (TLCA 1999, Zotero
`EBD23DBT`); *Call-by-Push-Value: A Functional/Imperative Synthesis* (2003,
Zotero `GMGNPJTX`); *Jumping Semantics for Call-By-Push-Value* (Zotero
`83ADMPBG`); *Adjunction Models for Call-By-Push-Value with Stacks* (Zotero
`3JTC3NB8`). ral side: RATIONALE §"Values and commands"; `docs/SPEC.md` §2,
§5.
