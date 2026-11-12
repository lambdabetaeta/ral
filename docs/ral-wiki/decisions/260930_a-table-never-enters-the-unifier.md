---
status: accepted
generated_at_commit: c4bd558f
generated_at_date: 2026-11-11
---

> **Amended 2026-11-11 — Written host options.** Host files follow SPEC
> §15.3: their options and nested tables are written records, checked key by
> key during generation, as at `within` and `grant`. This supersedes the
> return-ascription bullet below and the “Live return contracts” amendment:
> no inferred-row walk, return-ascription factory equations,
> queued nested checks, scratch table judgement or solver `Settled` phase.
> Fields are ordinary uses that may constrain binding and application
> matches and reach decoder admission sites before readback.
>
> A manifest factory is a written one-parameter block, one level deep.
> The rc writes all thirteen keys and every `limits:` and `theme:` key;
> `` `inherit `` and `` `set v `` state retention or override explicitly.
> `()` is empty options only without required keys, as in a grant profile.
> Collection keys remain data; written `handlers:` maps are checked arm by
> arm in separate regions everywhere, bound or computed maps by one equation.
> The rc and manifest decoders retain value-shape, domain and installation
> checks, not result-shape or keyset checks. The table still never becomes a
> unified record type; the row and spread decisions below stand.

> **Amended 2026-11-11 — Live return contracts.** Generate on `Solver`,
> settle every remaining elaboration decision into `Settled`, hold the return
> contract live, close gates, nested tables and errors into `Solution`, then
> freeze by readback. Skip the hold if settling reported errors. A contract
> is a use of the returned value: it can fix weak types, but cannot select a
> `let` mode or call split, enter a region or open a hole.
>
> Fields use `equate_held`; factory equations propagate through the same live
> solver. RC and manifest `handlers:` use `Holds::Handlers`, one equation
> against `Map(U([String] → Command))`, with no per-field arm hold or manifest
> dependency. Records are refused with the map spelling. These equations
> reach decoder admission sites before freezing; persistent installation
> still vets each handler. `within` checks a written map entry by entry for
> named diagnostics, any other value by the same equation, with no runtime vet.
>
> The walk re-reads row tails after each field. Afterwards, keysets are judged
> on final types and only fields unseen by the walk are held on scratch,
> without replaying live clashes. Contract equations introduce ground types
> apart from fresh variables used once, so those revealed fields need no
> further solving.
> There is no repeated inference or fixpoint. Nested tables remain queued
> scratch judgements at close, after live holds. Variable returns and
> unresolved field/member types retain runtime checks. The table itself
> still never becomes a unified record type; the scratch-only return
> ascription clause below is superseded.

# A table never enters the unifier

**A declared table never enters the unifier, and a row slot is a label and a
type.** A form's options are consulted key by written key at the form; a
contract file's return is ascribed against its table after inference
finishes; the runtime doors keep judging what no rule checked. With no table
left to type, presence flags have no purpose, so they go: a record's spread is
an *update* of one base, and rows are the textbook Rémy rows.

## What was decided

- **A form's options are syntax.** `within` and `grant` write their option
  names in their own bracket; each value is held at the type its table gives
  that name. No record type is built for the bracket, so a bound bundle, a map,
  a spread and a repeated name are parse errors, each with its own sentence.
- **A contract file is ascribed after inference.** An rc file, a plugin
  manifest and a capability profile are held to their table in a scratch copy
  of the unifier, on a finished result that flows nowhere else, as ML ascribes
  a signature (`ascribe` in `contract.rs`). A return typed at a variable, and a
  manifest factory, stay on the runtime door. `()` is the empty keyset.
- **The deletions.** `check_one_optional_type`, `condition()`, `Clash`,
  `ContractClash` (T0025, retired and never renumbered), and the gate in
  `typecheck()`. The door existed because declared tables were the only
  construct putting a ground payload beside a variable flag; once no table
  reaches the unifier, nothing does.
- **No presence flags.** `Row ::= Empty | Extend(Label, Ty, Row) | Var`. The
  `Field` enum, `PresenceVar`, the unifier's flag store and their traversals in
  generalisation, schemes, keys and printing are deleted. A field met against
  `Empty` is a direct error — *no field `l` in this record* or *this record
  needs `l`* — where it was a peel.
- **The spread is an update.** `[...$r, k₁: v₁, …, kₙ: vₙ]` takes one spread,
  first, with distinct `kᵢ`. Given `$r : [k₁: β₁, …, kₙ: βₙ | ρ]`, all fresh,
  the result is `[k₁: A₁, …, kₙ: Aₙ | ρ]`: a field may change its type, and a
  spread never adds one. The parser refuses a spread that is not first and a
  second spread.
- **The error says the three idioms.** `Reason::RecordUpdate`: *`$cfg` has no
  field `tokne` to update — a spread replaces fields its record already has
  (did you mean `token`?)*; when no field is within edit distance 2, it offers
  the idioms instead — give the record the field from the start, nest it
  (`[cfg: $cfg, total: …]`), or make `$cfg` a map (`[...$cfg, "total": …]`).

## Why

Flags bought two things: a type for a form's options, and a polymorphic type
for the put. The first is gone — options are syntax — and the second was
never used to add a field in code: the two real sites
(`examples/atomic-update`, `exarch/data/edit-replace.md`) update a field that
exists. The sites that extended a record were documentation and tests.

With flags, order-independence rested on *confinement*: every `Field::Var`
payload occurring nowhere but beside its own flag, an invariant two functions
had to re-establish and a cross-table condition guarded. Without them the
algebra is unitary on its own and the invariant has no referent. An explicit
field already beat a spread wherever it sat, so a trailing base
`[k: v, ...$b]` meant `[...$b, k: v]`; spread-first only respells it. Elm's
`{ r | k = v }` is the whole feature.

## Rejected

Scoped labels, and extension with flags. A record that needs a field it
lacks is a nested record, a field given from the start, or a map.

Supersedes [[decisions/260921_a-field-is-a-flag-and-a-type|a-field-is-a-flag-and-a-type]]
and [[decisions/260921_unitarity-lives-in-the-term-rules|unitarity-lives-in-the-term-rules]],
and narrows [[decisions/260912_absence-is-merged-not-defaulted|absence-is-merged-not-defaulted]]
further. See [[design/row-types|row-types]] and
[[internals/type-inference|type-inference]].
