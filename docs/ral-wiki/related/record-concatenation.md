---
verified_at_commit: a2f49715
verified_at_date: 2026-09-13
against: [design/row-types, invariants/fields-are-reached-by-name, decisions/260913_an-open-spread-must-come-last]
---

# Record concatenation — the operation ral declined

Wand 1991; Harper–Pierce 1991; Rémy 1992; Gaster–Jones 1996; Leijen 2005.

**Concatenation is the one record operation ral does not have, and every
published way of typing it charges a price ral has already declined to pay.**
A literal's row is iterated *extension*: where the parts' fields are known that
construction *is* their concatenation, and where they are not, ral refuses the
literal rather than approximating it
([[decisions/260913_an-open-spread-must-come-last|an-open-spread-must-come-last]]).

## The obstruction is the literature's, not this implementation's

- **No principal type.** Wand types concatenation only by inferring a *set* of
  types, one per way the operands might overlap. ral's own witness is an
  instance: `ρ ++ [l: Int]` against `[l: α | ρ']` has two incomparable
  solutions — `l` from the left operand, or `l` from the right with `ρ` lacking
  it — so a concatenation constraint has no most general unifier and row
  unification would stop being unitary.
- **Three published prices, and ral has refused each already.** Harper and
  Pierce make concatenation symmetric and pay in explicit compatibility
  constraints; Gaster and Jones pay in `lacks` predicates carried by qualified
  types ([[related/scoped-labels|scoped-labels]] is where ral bought out of
  that tax); Rémy shows concatenation costs nothing *extra* — but only once
  presence/absence flags are already in the type language, which is the
  precondition, not the discount.
- **Leijen has no concatenation at all**, only extension. ral implements
  Leijen's calculus, so the fence ral meets is that calculus's own boundary
  rather than a gap in ral's unifier.

## What the refusal buys, and what it costs

- **Buys:** plain Hindley–Milner ([[design/types|types]]), unitary row
  unification, and no predicate slot in a `Scheme` — which serialises
  ([[invariants/schemes-leave-closed|schemes-leave-closed]]), so a predicate
  would become persisted-format surface, versioned for the life of the language.
- **Costs:** exactly one idiom — a default merged behind a record whose fields
  are not known where the merge is written. Absence over such a record travels
  as a variant instead
  ([[invariants/optionality-via-variants|optionality-via-variants]]).
- **The direction is reversible.** Every program ral accepts stays accepted if
  flags are ever adopted, so the restriction can be lifted without breaking a
  program; a weaker typing could only ever be withdrawn.

## Why no deletion rescues the idiom

The restriction is the row algebra's, so nothing removable lifts it:

- **Reading a label requires naming it in a row**
  ([[invariants/fields-are-reached-by-name|fields-are-reached-by-name]]), and
  naming it in the result's row forces every caller to supply it — which is
  what makes the entries behind an open spread dead under every sound typing.
- **The weak typing is unsound, not merely useless.** Typing the merge at `[ρ]`
  and forgetting the trailing fields would let `ρ` instantiate to `Empty`, so a
  closed record type would describe a value carrying more fields than it names.
  Closed rows are exact — structural equality compares a record's key–value
  pairs (`docs/SPEC.md` §4.8) — and un-exacting them is width subtyping, a
  different calculus rather than a deletion.

## What ral could borrow

- **The predicate route with a modern solver**, if the idiom ever must exist:
  PureScript keeps concatenation by carrying a `Union` constraint the compiler
  discharges when enough of the three rows are known, with functional
  dependencies determining the third from two, and deferring it otherwise. That
  is Gaster–Jones with better ergonomics, and it charges the same predicate slot
  priced above.
- **Rémy's flags** stay costed in
  [[decisions/260913_an-open-spread-must-come-last|an-open-spread-must-come-last]]:
  not additive over duplicate-retaining ordered rows, since selecting `l` with
  its flag unknown is stuck.

## See also

[[design/row-types|row-types]] (the chain argument),
[[related/scoped-labels|scoped-labels]] (the calculus ral implements),
[[invariants/fields-are-reached-by-name|fields-are-reached-by-name]] (the
condition the refusal's completeness rests on), `docs/SPEC.md` §4.5.

Cite: Wand, *Type inference for record concatenation and multiple inheritance*,
Information and Computation 93(1), 1991 (LICS 1989), Zotero `J8EB9KHC`; Harper
and Pierce, *A record calculus based on symmetric concatenation*, POPL 1991;
Rémy, *Typing record concatenation for free*, POPL 1992; Gaster and Jones, *A
polymorphic type system for extensible records and variants*, Nottingham
NOTTCS-TR-96-3, 1996; Leijen, *Extensible records with scoped labels*, TFP 2005,
Zotero `AKRCBMLD`.
