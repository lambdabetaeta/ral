---
verified_at_commit: 7ba500b
verified_at_date: 2026-06-17
against: [design/row-types, internals/type-inference, design/scoping]
---

# Scoped labels — the record calculus ral implements

Leijen, *Extensible records with scoped labels*, TFP 2005.

**ral's records are Leijen's calculus taken nearly whole — duplicate labels
retained in value and type, first-match selection, free extension as
prepend-and-shadow — minus one primitive: restriction.** Leijen gives three term
operations (extension, selection, restriction `r − l`); ral keeps two. Every
divergence below traces to that cut.

## What ral takes whole

- **Scoped labels.** Duplicates are permitted and *retained*, in the value and
  in the type; selection takes the first. This is Leijen's defining move against
  the earlier free-extension systems (Wand, Rémy), where extension *overwrites*
  — the ambiguity that made Wand's system incomplete, and that Rémy's
  presence/absence flags (`pre`/`abs`) repair at the price of flags in every
  type ([[design/row-types|row-types]]).
- **Free extension is spread shadowing.** `[...$base, port: 9090]` is iterated
  `{l = e | r}`: prepend, shadow, never remove.
- **The equality and the unification, unchanged.** Rows are equal up to
  permutation of *distinct* labels (*eq-swap* demands `l ≠ l′`), so same-label
  order is preserved and shadowing is coherent. ral's `unify_row` is Leijen's
  *(uni-row)*: rewrite the row to expose the sought label, minting a fresh
  shared tail when the tail is a variable; the row-spine occurs check on the
  tail variable ([[internals/type-inference|type-inference]]) is Leijen's
  termination side condition `tail(r) ∉ dom(θ₁)` — the one TREX got wrong.
  RATIONALE names the permutation the Rémy rewrite; it is Leijen's Figure 3.
- **No type-system tax.** The selling point ral bought: no *lacks* predicates
  (Gaster–Jones), no flags (Rémy) — a new mono-type equality plus the extended
  unifier, orthogonal to the rest of the type system. This is what lets
  [[design/types|types]] stay plain Hindley–Milner.
- **Variants over the same rows.** Leijen's §5 duals; ral's open variants are
  the same row discipline on the choice side, carrying optionality
  ([[invariants/optionality-via-variants|optionality-via-variants]]).
- The value level mirrors the type equality: closed-record structural equality
  is insensitive to the order of distinct labels.

## Divergences (deliberate)

- **No restriction operator.** Leijen's `r − l` removes the first occurrence,
  and update `{l := x | r}` and rename each derive from it in one line. ral cut
  it (`docs/SPEC.md` §20.7: shadowing "without needing a restriction
  operator"), so override is shadowing and a shadowed field is unreachable —
  there is no un-shadow. Leijen's own motivating idiom — override an
  environment's `color`, then restrict to re-expose the parent's — is therefore
  inexpressible at the record level; ral does that layering on the dynamic
  context instead, where frames pop by scope exit rather than by a term
  operator ([[design/scoping|scoping]]).
- **Two label alphabets.** Leijen runs records and variants over one label
  namespace; ral splits bare labels (records) from backtick labels (variants),
  and the two never unify.
- **Multi-spread is imprecise.** Leijen types iterated extension exactly; ral
  threads a *single* spread's field types precisely, while several spreads in
  one literal yield an open but imprecise result (`docs/SPEC.md` §20.7).

## What ral could borrow

- **Restriction, if update or rename is ever wanted.** Its type
  `[l: α | ρ] → [ρ]` is already expressible in ral's rows, and both derived
  forms come free. The cost is re-admitting un-shadowing at the data level —
  the idiom ral deliberately routes through `within` instead.
- **The shadow warning.** For a record of *fixed* type carrying duplicate
  labels, Leijen suggests a shadowed-variable-style warning — a warning, not an
  error, since a program with duplicates cannot go wrong. A cheap checker lint.
- **The implementation menu.** Labeled vectors with compiler-folded offsets
  give constant-time selection even on open rows; extension predicates `l|r`
  are always solvable (unlike *lacks*), so they never surface in types. Worth a
  look if record performance ever matters.

Restriction and the shadow warning were rejected on review
([[decisions/260603_related-borrowables-rejected|related-borrowables-rejected]]);
the implementation menu stands as reference.

Cite: Leijen, *Extensible records with scoped labels*, Trends in Functional
Programming 2005. Zotero `AKRCBMLD`. ral side: RATIONALE §"Record types and
scoped labels"; `docs/SPEC.md` §20.7.
