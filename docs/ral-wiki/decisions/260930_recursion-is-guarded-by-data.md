---
status: accepted
generated_at_commit: 8d868e18
---

# Recursion is guarded by data

**A recursive type must pass through a list, map, record or variant.** Binding
a type or computation variable `α` to a structure `τ` is refused, as
`CyclicType` (T0073), exactly when `α` is reachable from `τ` along `U`, `→`,
`F` and `Handle` alone, following bindings as the search goes.

## What was decided

- **The search sits at the two bind sites**, `bind_ty` and `bind_comp_ty` in
  `unify.rs`. A variable–variable union cannot close a cycle: both sides are
  resolved first, so a structure on either side makes the step a bind.
- **It never crosses a data edge**, so it needs no path bit and a visited set
  keyed by root is sound. The invariant keeps the non-data subgraph acyclic, so
  the search is finite.
- **The sentence names the edge** by which the search met the variable: a
  function *is applied to itself*, *takes itself as an argument*, or *returns
  itself*.
- **A cyclic scheme prints as `μβ. Map β`**, the μ binding the root at the
  node equal to its binding, not as `∀α. [α]`.
- **Unchanged:** `Pairs`, the cycle-aware traversals, cyclic roots in schemes.
  The unifier stays equi-recursive, for data cycles.

## Why

Real programs need recursive data: json-get's `μβ. Map β`, a stream (a variant
with a thunk tail), a tree of records over lists. A cycle through arrows alone
is untyped λ-calculus: `w $w`, a function that returns itself, or `$g $g` from
a forgotten argument — and the checker used to accept these silently, since it
had no occurs check at all.

OCaml's default admits cycles only through polymorphic variants and objects.
This widens "variants" to all data, because json-get's cycle goes through a map
and a tree's through a record and a list.

## Rejected

A full occurs check, which refuses streams and json-get; and unrestricted
recursion, which types every self-application.

Refines [[decisions/260606_unify-one-sided-obligations|unify-one-sided-obligations]],
whose "no occurs check" is now "no occurs check through data".
