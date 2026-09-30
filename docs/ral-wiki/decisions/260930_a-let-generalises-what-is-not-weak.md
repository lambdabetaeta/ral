---
status: accepted
generated_at_commit: 8d868e18
---

# A `let` generalises what is not weak

**Every `let` generalises every variable of its result type that is neither
free in Γ nor weak.** A *weak* variable has one type for the whole unit; there
is no value restriction.

## What was decided

- **Weakness is a permanent mark on a variable of any sort** — type, row or
  computation. `Unifier::mark_weak` sets it; `unite` hands it to the survivor, and a
  variable of any sort unified with a weak one, even one already fixed to a
  structure, becomes weak, so a later mismatch is still seen; binding a weak
  variable to a structure makes every variable of the structure weak, with the
  same source, since each occurs in it after resolution. Nothing unsets it.
- **`generalize` subtracts weak variables** as it subtracts Γ's free variables,
  and the scheme records them (`Scheme.weak`) instead of quantifying them. A
  scheme prints them `_α`, after its binders: `∀α. [a: α, b: _β]`.
- **`fv⁺` includes them.** A pending label read whose target is weak is kept by
  the boundary that would have closed it as a record, and is closed at the
  unit's end (Amendment A of the plan).
- **A stored scheme marks its residuals, and the next unit re-seeds each as a
  fresh weak variable** (`reseed_weak`, in `seed_env`), so a residual is never
  read as a foreign id that `Store::find` would alias. A unit settles its
  `Define` schemes (`settle_weak`) before they are stored, so a residual the
  unit fixed is stored as what it was fixed to.
- **A mismatch that meets a weak variable, or the type one was fixed to, carries
  a note** saying that the type is shared by every use in its unit and that
  another use fixed it.
- **Two things are weak.** A computed index's container, key and element types
  are, from the moment the index is read, whether or not it settles
  ([[decisions/260930_operators-are-kinded|the one relation]]); the result of a
  checked boundary is
  ([[decisions/260930_a-boundary-is-checked-against-its-type|a-boundary-is-checked-against-its-type]]). A scheme that settles a weak variable into a
  cycle keeps the cycle: `settle_weak` snapshots it like a quantified one.

## Why

A checked boundary is only as good as the type recorded there. With plain
generalisation `let doc = from-json …` would be `∀α. α`, and no use would reach
the door that admits the value; with a weak variable it is `_α`, one type per
unit, and the door is held to every use.

The value restriction is not needed for this. ML restricts generalisation
because `ref [] : ∀α. α list ref` would let one cell be written at one type and
read at another. ral has no first-class reference cell, and every other way a
value of a type the program did not decide enters typed code is a boundary,
whose result variables are weak. So the restriction would guard nothing, and
would cost `let f = map $g` its polymorphism and refuse a module used at two
types by one importer. If ral ever gains a reference cell the restriction comes
back with it; the perimeter test of the boundary slice is what says it has not.

Making the mark permanent is what keeps the verdict independent of statement
order. Were it lifted when an index settled, whether `let h = { |c| $c[$i] }`
was generic would depend on what earlier statements fixed about `$i`; with the
mark permanent it depends only on the syntax of the `let`'s body. The index is
therefore weak even where its target is already a list, so that a helper indexing
a known list and one indexing a not-yet-known one are treated alike.

## Rejected

The value restriction. Generalising a boundary's result, as today.

Refines [[decisions/260603_session-scheme-continuity|session-scheme-continuity]]:
a stored scheme is closed up to its weak residuals
([[invariants/schemes-leave-closed|schemes-leave-closed]]).
