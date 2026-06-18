---
verified_at_commit: a590f4f
verified_at_date: 2026-06-18
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

- **`F` is graded.** ral's computation type is `F[I,O] A`: Levy's `F A`
  indexed by two byte-channel modes (`∅` / `Bytes` / `μ`). The grading is what
  makes a pipeline edge a typing fact — adjacent stages connect only when the
  modes agree ([[design/types|types]],
  [[decisions/260601_modes-equality-constrained-shared|modes-equality-constrained-shared]]).
- **The pipe is a new computation combinator.** CBPV composes computations by
  sequencing and application only. ral adds `|` — binary composition along the
  byte channel, run as a process group or a fold by edge type
  ([[design/pipelines|pipelines]]). It is not a CBPV connective; it is exactly
  where ral is a shell rather than a λ-calculus.
- **No computation products.** ral's computation types are `F[I,O] A` and
  `A → B`, full stop; Levy's `Πᵢ Bᵢ` is absent. Where it would be used, a
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
`3JTC3NB8`). ral side: RATIONALE §"Values and commands"; `docs/SPEC.md` §0,
§3, §4.
