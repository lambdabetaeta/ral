---
verified_at_commit: a590f4f
verified_at_date: 2026-06-18
against: [design/effects-handlers, design/row-types, design/types, design/scoping]
---

# Rows and handlers — the effect typing ral declined

Hillerström, Lindley, *Liberating Effects with Rows and Handlers*, TyDe 2016.

**The path ral deliberately did not take: the row machinery ral reserves for
data, extended to effect types.** Their thesis is that "the abstraction required
to implement extensible effects and their handlers is exactly row
polymorphism" — in Links, one Rémy-style row system types polymorphic variants,
records, *and* every function arrow's effect signature. ral has both halves —
the row unifier ([[design/row-types|row-types]]) and the handler stack
([[design/effects-handlers|effects-handlers]]) — and keeps them apart: rows
type records and variants, never computations.

## Correspondences

- **The runtime is nearly the same machine.** Their interpreter keeps a
  first-in-last-out handler stack; an operation unwinds to the innermost
  handler whose clauses mention it, forwarding through the rest (the
  `ℓ ∉ BL(E)` side condition); handlers are deep — the clause's continuation is
  re-wrapped in the same handler. That is ral's
  [[internals/handler-dispatch|handler-dispatch]]: innermost-first lookup,
  frame fall-through, deep frames. The machines part only at the reify point —
  their generalised CEK captures the unwound frames as a first-class `k`; ral
  resumes in place and never reifies.
- **Equations are dropped on both sides.** "As with most other implementations
  of effect handlers, we do not consider equations" — ral agrees, with the
  sharper reason that its external-command theory is free
  ([[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]]).
- **Both substrates are Levy-lineage.** λρ_eff is fine-grain call-by-value over
  an A-normal IR; ral is full CBPV. And both type systems are plain
  Hindley–Milner plus rows — their observation that row polymorphism
  "integrates rather smoothly" with HM is the same fact ral's `unify_row`
  proves in-house, for data.

## Divergences (deliberate)

- **Effects in types vs effects in scope.** The paper's point is static effect
  rows on every arrow — `{Move:(Player,Int) {}-> Int|e}->` — with presence
  types (`Move-`, `Cheat{p}`) making handler pipelines typecheck by row
  unification. ral tracks no effect in any type: the operation namespace is the
  open external-command surface, authority is dynamic
  ([[design/scoping|scoping]], [[design/grant|grant]]), and a function type
  never says what it performs — the *requirement half* ral lacks by choice
  ([[related/system-c|system-c]]).
- **The wild inversion.** In Links the intrinsic effects — I/O, randomness,
  divergence — are the *wild* row entry (`~>`): unhandleable, interpreted only
  by the interpreter, while user-declared abstract operations have no meaning
  until handled. ral inverts both halves: its handleable operations *are* the
  world effects, each with a default interpretation (the OS performing the
  syscall), and there are no meaningless abstract names. In Links a handler
  *confers* meaning; in ral it *shadows* one
  ([[design/syscalls-are-effects|syscalls-are-effects]]).
- **Even the row flavour differs.** Links rows are Rémy's — distinct labels
  with presence information; their §6 places Koka and Frank on the other side,
  "allow[ing] multiple occurrences of the same label in a row". ral's rows are
  that other side ([[related/scoped-labels|scoped-labels]]) — and aptly so:
  in a scoped-label effect row, a duplicate label is a *shadowed outer
  handler*, which is precisely ral's handler-stack discipline. Were ral ever to
  type its dynamic context, Koka shows the scoped flavour fits it natively.
- **Multi-shot, parameterised, return clauses** — their game-tree handler maps
  `k` over all moves; `allResults` calls it twice; `state` curries parameters
  through it. The same cut as against Plotkin–Pretnar applies; ral keeps the
  implicit exactly-once tail resumption. Their own survey legitimises the
  position: Multicore OCaml drops effect types and restricts continuations to
  one-shot for efficiency, and their future work reaches for linearity to
  avoid copying — ral sits at the far end of that same axis, where the
  continuation is never even materialised.

## What ral could borrow

- **The existence proof for the requirement half.** Should ral ever want
  static effect tracking, this paper shows the machinery is already in the
  building: one row unifier serving records, variants, and effects, smoothly
  inside HM inference. The cost is not implementation but signatures — and the
  natural ral flavour would be scoped-label effect rows à la Koka, mirroring
  the handler stack, not Links' presence types. Rejected on review
  ([[decisions/260603_related-borrowables-rejected|related-borrowables-rejected]]).

Cite: Hillerström & Lindley, *Liberating Effects with Rows and Handlers*, TyDe
2016, [doi:10.1145/2976022.2976033](https://doi.org/10.1145/2976022.2976033).
Zotero `6EEPMYUD`.
