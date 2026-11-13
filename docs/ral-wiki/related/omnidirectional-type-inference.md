---
verified_at_commit: 64b60eafa2eec1a958ff29d8671376d79856dc3a
verified_at_date: 2026-11-10
against: [design/types, design/capture, internals/type-inference, decisions/261110_elaboration-is-decided-by-suspended-matches]
---

# Omnidirectional inference: suspended matches without partial schemes

**ral adopts suspended matches, not the paper's full omnidirectional solver.**
Alistair O'Brien, Didier Rémy and Gabriel Scherer, *Omnidirectional type inference
for ML: principality any way*, [arXiv:2511.10343](https://arxiv.org/abs/2511.10343).
This comparison reads the [paper's HTML](https://arxiv.org/html/2511.10343),
especially §§2.4–2.5, 4.1, 5 and 6, against ral's
[[decisions/261110_elaboration-is-decided-by-suspended-matches|decision]] and
[[internals/type-inference|inference algorithm]].

## The mechanism adopted

A *suspended match* waits for the known outer constructor of a type, then emits
the uniquely selected branch's constraints (§2.4). It does not guess a branch
from whichever complete solution would make that branch work.

- §4.1's contextual semantics requires the surrounding constraints to determine
  the shape. Its erasure replaces suspended matches with `true`, preventing a
  branch from manufacturing its own evidence.
- §5's `Uni-Type` observes a non-variable type; `Uni-Var` observes a variable
  equated by its context to a non-variable type. These are the sources of known
  heads ral uses: unification fixes a computation head or binding grade before
  the corresponding match is discharged.
- ral's two matches concern CBPV elaboration, not nominal field overloading:
  `bound` selects run/thunk/capture at `let`; `apply` selects an arrow step or
  a returned-thunk split. The equations and eventual term come from the same
  branch rule ([[design/capture|capture]]).
- ral adds a pairing-specific improvement: `F^ν A` with `A` excluding `Unit`
  entails `ν = p`. A binder's demanded result type does not select capture;
  reverse reasoning from that branch result is excluded.

## What is deliberately excluded

**A use cannot choose the elaboration of an already generalised definition.**
This is the limit of the adoption, not an implementation of the paper with a
few optimisations omitted.

- The paper's §2.4 example `getx` needs an instance in the let-body to reveal
  the unresolved definition's record shape while other parameters remain
  polymorphic. *Incremental instantiation* copies partial type schemes and
  revisits their instances as the schemes are refined.
- §5's `Uni-BackProp` propagates a known shape from an instance back to a
  regional variable in its definition. ral excludes this rule, incremental
  instantiation and partial schemes. Ordinary instantiation freshens a completed
  quantified prefix; it is not a persistent link to the definition.
- §6 explains why partially generalised regions require a tree rather than
  integer levels alone: an unresolved region can remain alive across sibling
  regions. ral closes every young match before generalisation; old matches
  lower all their mentions. Its regions therefore remain a stack and ranks
  suffice. This is a stronger lifetime restriction, not a criticism of the
  paper's region representation.
- This excluded instance-to-definition propagation is distinct from using
  ordinary equations elsewhere in the same region to fix a match's scrutinee.
  The latter is precisely why ral suspends matches.

## Defaults and claims

**ral chooses defaults at a region boundary; the paper's formal development
does not supply those defaults.**

- §2.5 explicitly leaves default rules unformalised, while reporting optional
  default strategies in the prototype. It would be incorrect to say the
  implementation has no defaults.
- ral defaults unresolved application to an arrow, and binding to a `p`
  producer. It chooses all defaults for a closing rank together and rejects
  incompatible binding/application defaults on one unknown. Rank zero closes
  the whole unit.
- These defaults select an elaboration; the paper's principality results are
  not thereby results about ral. Order independence is the intended property
  of ral's restricted solver, with behavioral validation and final review still
  pending.

The shared lesson is to make shape-sensitive decisions explicit constraints.
The boundary is equally important: ral executes a unit's single elaborated
body once closed, rather than preserving an unresolved definition for later
instances to refine.
