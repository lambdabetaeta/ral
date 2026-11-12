---
status: active
generated_at_commit: c0905f5e
verified_at_commit: c0905f5e
verified_at_date: 2026-11-12
anchors: [Generator, Solver, Solution, Readback, Phase, Inferring, BindHole, CallHole, Mode, generalize, Closed]
---

# Elaboration is decided by suspended matches

**Generation cannot observe a solution; elaboration waits for a known head.**
Capture and returned-thunk application change the checked program, so neither
may be chosen from the store's state at the moment a syntax rule visits it.

## Decision

- Generation emits equations, regions, instantiations, obligations and two
  *suspended matches*: `bound τ C` chooses a binding's adaptation; `apply C A D`
  chooses whether application must force a returned thunk.
- A known head selects a branch. An unresolved computation or binding grade
  waits under its root and is re-stepped when the store touches that root.
  Global producer improvement entails `p` for returner and handle
  payloads whose head or kind excludes `Unit`, by `F^w A ⇒ A = Unit`;
  this is entailment, not defaulting.
- A match reads only its scrutinee, not its result. In particular, demanding a
  `String` binder cannot select capture. Branches do not propagate information
  backwards to justify their own scrutinees.
- Closing a region defaults its remaining young matches together: `bound`
  chooses a value producer, `apply` an arrow. If both wait on one unknown,
  their defaults disagree: refuse with both uses (`ConflictingDefaults`, T0080),
  rather than give either priority.
- Holes are annotations of the IR's inferring phase: a binding carries a
  `BindHole`, a call a `CallHole`; the checked phase has none. Deciding or
  defaulting a match records its decision on its hole: a binding's `Mode`, an
  argument's splits. Only after successful unit closure, with every hole
  decided, does readback apply those decisions, snapshot sites and settle
  exported schemes into `Closed`. A refused unit exports neither a checked
  tree nor a scheme. Gates read the final head; diagnostics
  are sorted stably by source span, not by solver timing.

## Generalisation is stated by rank

**A rank is the depth of the region that owns an unknown.** This restates
[[decisions/260930_a-let-generalises-what-is-not-weak|a let generalises what is not weak]]
without an environment scan or a separate weak mark.

- Fresh unknowns take the current rank; unification keeps the minimum and
  lowers every reachable unknown. A suspended match lowers everything it
  mentions to its scrutinee's rank.
- A `let` opens a region around its RHS and pattern, closes it, then quantifies
  reachable unknowns younger than the enclosing rank. Parameters of enclosing
  lambdas are older; unresolved matches on older scrutinees remain suspended.
- A boundary result is lowered to rank zero. No `let` quantifies rank zero:
  such a result has one type per unit. `WeakSource` records provenance, and a
  scheme's `Weak` roles record value/computation/row residuals for cross-unit
  reseeding. Rank-zero grades remain residual until settlement; a scheme's
  grades are only ever quantified.
- An unresolved producer grade lowers its payload dependency to its rank,
  not conversely: a weak, Unit-capable payload does not prevent a fresh grade
  from generalising. Structural root-to-reachable lowering is unchanged,
  including grades inside weak returned thunks. Young pairs retire before
  generalisation; fresh instances register their own pairs, so improvement
  does not mutate quantified definitions.
- No successful region leaves a young suspended match. Schemes are not partial.
  A recursive group is checked once in a region of its own, and each projection
  instantiates its member's generalised scheme; so regions form a stack, and
  integer ranks suffice. There is no value restriction.

## Scoped amendment to the carried unifier

**The unifier cannot be carried unchanged where it rejects productive field
recursion.** The same regular record type was accepted or refused depending on
equation order and anchor placement. This contradicts the existing
[[decisions/260930_recursion-is-guarded-by-data|data-guarded recursion]] rule,
not the generation/solving/readback separation.

- Row occurs checking follows only the finite tail spine. Binding gives each
  structured field payload a type alias, reusing variable payloads, so every
  admitted cycle has a type/computation anchor. Schemes and sites need no row
  μ-binder ([[design/row-types|row types]]).
- Depth and deep-data checks precede aliasing; rank lowering and weak provenance
  traverse the stored payloads. Rémy's shared-tail label-multiset safeguard and
  duplicate-label order remain intact.
- This is a semantic correction of erroneous refusals, not just a diagnostic
  change. `tests/accept/a-record-in-its-own-field-is-productive.ral` records
  the accepted source case.

## Precedent and limits

The mechanism is to emit constraints, defer undecided conclusions, and
discharge them before their variables generalise; arms and arguments here still
unify without adaptation.

The suspended-match mechanism follows
[[related/omnidirectional-type-inference|O'Brien, Rémy and Scherer]]. Incremental
instantiation, partial schemes and instance-to-definition shape propagation
are excluded: each definition has one elaborated body, closed before execution.
Defaults are ral's policy, not a principality result inherited from that paper.

The intended order-independence argument separates generation, monotone solving,
rank closure and readback. See [[internals/type-inference|type inference]] for
what the regressions cover.

This is neither a complete confluence proof nor a completed mechanisation, and
diagnostics are not claimed order-independent.

## Where

- `core/src/ir.rs` (`Phase`, `Explicit`) and `core/src/typecheck/env.rs`
  (`Inferring`): holes as phase annotations.
- `core/src/typecheck/generate.rs` and `generate/`: constraint generation.
- `core/src/typecheck/solve.rs`: private store, suspensions keyed by scrutinee
  root, propagation, recorded decisions, region defaults and unit closure.
- `core/src/typecheck/readback.rs`: elaboration from a closed `Solution`'s
  decisions.
- `core/src/typecheck/{unify,generalize}.rs`: ranks and quantification.
- `core/src/ty/scheme.rs`: per-sort `Scheme` roles and `Closed`.

See [[internals/type-inference|type inference]], [[design/capture|capture]] and
[[invariants/schemes-leave-closed|schemes leave closed]]; `docs/SPEC.md` §§17.3–17.7.
