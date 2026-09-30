---
verified_at_commit: 8d868e18
verified_at_date: 2026-09-30
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
  ([[design/types|types]]); the variables of data of an undecided shape are
  weak instead, and a `let` leaves them alone. The substrate does real type-theoretic work.
- **The subsumption is live, not historical.** Eager application is the
  call-by-value image; passing a `{M}` thunk recovers a call-by-name call site
  term-by-term. Both disciplines are expressible and neither is baked in —
  which is Levy's theorem used as a language-design budget.
- **The machine is the CK reading, now literally a CEK machine.** ral's
  evaluator steps a focus ⟨M, E⟩ against a stack of frames, each frame
  carrying the environment it resumes under
  ([[internals/evaluator-machine|evaluator-machine]]). A tail call pushes no
  frame — β binds the argument into the closure's own environment and puts
  the body in focus, so depth is exactly `stack.len()`. That is Levy's
  jumping intuition — *calling a procedure is a jump, and returning is also a
  jump* — with the stack discipline enforced by the module's two doors,
  `evaluate` and `apply`, rather than by a calculus of stacks.

## Divergences (extensions, mostly)

- **`F` carries its value type and nothing else; a command is `F Unit`.** ral's
  returner is Levy's `F A`. A command — an external, a builtin that writes, a
  handler arm standing in for one — writes to stdout and returns `()`, as `cd`
  does, so `{ echo hi }` is `{Command Unit}` and `{ 'hi' }` is `{Command String}`.
  That `let x = hostname` binds the text `hostname` writes is no fact about
  types: it is the capture coercion `cap M to d. decode d`, which the checker
  wraps around the command a `let` can see, decided from syntax before any
  type is inferred
  ([[decisions/260930_capture-is-decided-by-syntax|capture-is-decided-by-syntax]],
  [[design/types|types]]). The calculus under it is Levy's unchanged.
- **The pipe is a new computation combinator, and it is not a typing fact.**
  CBPV composes computations by sequencing and application only. ral adds `|`,
  whose static rule says just that a stage feeds the next by writing — `Γ ⊢ M : F Unit`
  and `Γ ⊢ N : F B` give `Γ ⊢ M | N : F B`, the value the final stage's. What the
  combinator *does* is operational: it connects `M`'s stdout to `N`'s stdin with an
  operating-system pipe. A ral-written
  stage runs its own CEK machine on an OS thread, in a child `Shell` seeded
  from the parent's session; only an external command is a process, and
  only externals are members of the pipeline's process group, which the
  parent shell itself is not
  ([[design/pipelines|pipelines]],
  [[decisions/260902_stages-are-threads|stages-are-threads]]). It is not a
  CBPV connective; it is exactly where ral is a shell rather than a
  λ-calculus, and the honest
  reading is that the shell's one composition operator lives outside the
  calculus rather than being encoded into it.
- **No computation products.** ral's computation types are `F A` and
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
  which the frame stack and `Settled<Value>` become statements about stacks,
  and the machine's correctness a simulation result — the jumping-semantics
  paper is the bridge.
- **The βη-theory for the pure fragment.** [[design/cbpv|cbpv]]'s "equational
  reasoning in the pure fragment" can cite CBPV's equational theory verbatim
  rather than re-deriving it.

Cite: Levy, *Call-by-Push-Value: A Subsuming Paradigm* (TLCA 1999, Zotero
`EBD23DBT`); *Call-by-Push-Value: A Functional/Imperative Synthesis* (2003,
Zotero `GMGNPJTX`); *Jumping Semantics for Call-By-Push-Value* (Zotero
`83ADMPBG`); *Adjunction Models for Call-By-Push-Value with Stacks* (Zotero
`3JTC3NB8`). ral side: RATIONALE §"Values and commands"; `docs/SPEC.md` §2,
§5.
