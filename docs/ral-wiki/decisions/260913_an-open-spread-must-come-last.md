---
status: superseded
generated_at_commit: 07b87759
superseded_by: decisions/260921_a-field-is-a-flag-and-a-type
---

# An open spread must come last

> **Superseded** by
> [[decisions/260921_a-field-is-a-flag-and-a-type|a-field-is-a-flag-and-a-type]].
> A record literal is no longer the concatenation of its parts but a **put**
> over one base, so there is no last part for an open spread to be, `T0023` is
> gone, and a second base is refused at parse instead. What that decision
> bought — a presence flag per field — is the very thing this page records as
> rejected, and the price it feared is paid differently: the flag is a fifth
> unifier sort, but absence is remembered *at a type* (`δ_l`), which is what
> keeps the algebra unitary and the `Scheme` free of a predicate slot. The
> reading below is kept for its account of why concatenation has no mgu, which
> stands.

**A record literal's row is the concatenation of its parts, and a spread whose
own row is still open may therefore be the last part only.** Anything written
behind such a spread is refused (`T0023`) rather than typed, because nothing
behind it could ever be read.

## Context

A literal with two or more spreads discarded every spread's row and returned a
fresh row variable. A free row variable satisfies any demand, so

```ral
let r = [dummy: 0, ...[flag: 1], ...[other: 2]]
if $r[flag] { … }
```

passed `--check` and failed at run time with `if: expected Bool, got Int '1'`.
The arms for zero and one spread were exact; the third was not an
approximation but a hole, and it shipped in the initial release, when an
all-spread literal still classified as a list and the arm was near-unreachable.
[[decisions/260912_absence-is-merged-not-defaulted|absence-is-merged-not-defaulted]]
then pointed the language's whole defaults mechanism at the two-spread merge,
`[tier: 'a', ...$given, ...$dflt]`.

## Decision

**The row is folded from the literal's low-precedence end.** An explicit entry
beats every spread and an earlier spread a later one — the evaluator's own two
passes — and selection-takes-first turns chain position into precedence. A
spread whose fields are known splices in exactly them. Zero and one spread stop
being special cases and become instances.

**An open spread with anything behind it is refused.** A row is a chain with
one open end ([[design/row-types|row-types]]), so nothing can be appended after
an unknown remainder. Under every sound typing those entries are dead: the only
one available binds that row to the fields behind it, which forces every caller
to supply them, so a default among them can never win. What refusal costs is
therefore precisely the programs whose only type is the concatenation type — a
merge behind an open spread, followed by a read of one of the defaults, does run
and is refused because no row ral can write describes its result
([[related/record-concatenation|record-concatenation]]). The error names the
fields that would have been unreachable:

```
[T0023] this spread has to come last, because nothing here says what fields it has
   Help: Nothing here says which fields this record has. If it has 'host' or
   'port' — written after it — this spread wins, and what you wrote there is
   never read. Move the spread to the end, or write out the fields you need
   from it.
```

Where the entries behind end in a *second* open spread, no order exists and the
help says so instead, naming the fields as the only way through.

The mirror order stays legal and exact: `[x: 0, ...[tag: 1], ...$g]` places the
known chain first and lets `$g` be the open tail.

**Reversing the precedence convention would not help.** Under last-wins the open
spread would have to come *first* instead — still at the row's open tail, still
the entry every other one beats. Either convention seats the unknown operand at
the low-precedence end, while a default needs the unknown operand to win: the
idiom is foreclosed by the row's single open end, not by the direction
precedence happens to run.

**A label written twice in one literal is refused** (`T0022`). A duplicate two
spreads bring together is *composition*, and resolving it by position is the
merge idiom's whole point; a duplicate one author writes twice in one literal can
only be a mistake, and refusing it is the record-side mirror of a rule ral
already keeps — a repeated `case` arm is refused by the parser, which needs no
types to see it. Position therefore means one thing throughout. Computed keys
keep the runtime warning, since the checker cannot see them.

## Consequences

**Defaults cannot be merged behind an unknown record, in any spelling.** A
map-pattern default fails by the same argument — it must either force the field
present, killing the default, or leave its type unconnected, which is the hole
[[decisions/260912_absence-is-merged-not-defaulted|absence-is-merged-not-defaulted]]
records. That decision's deletion of map-pattern defaults stands; its claim
that a merge *replaces* them was too broad, and held only for known records.
Defaults are assembled where the records are known; absence over an unknown
record travels as a variant
([[invariants/optionality-via-variants|optionality-via-variants]]).

**What counts as open is settled by the call, not by source order.** A block's
parameter is bound by the call the block appears in, so `map { |x| [...$x, …] }
$xs` spreads a record whose fields the list literal knows. The inferencer used
to check that body while the element type was still free and refuse it; it now
checks a block argument after the spine is unified and pushes the expectation
inwards ([[internals/type-inference|type-inference]]). The rule below is
unchanged — it simply stopped firing on programs whose rows were never open.
What remains refused is the row that no call can close: a parameter nothing
determines, as in `{|a b| [x: 0, ...$a, ...$b]}`.

**Two open spreads no longer unify.** `{|a b| [x: 0, ...$a, ...$b]}` was inferred
at `∀ρ. [ρ] → [ρ] → [ρ]`, a contract no caller wants; it is now refused at the
literal instead of blaming an argument.

**Refusal is the reversible direction.** Every program ral accepts stays accepted
if presence flags are ever adopted, so the restriction lifts without breaking a
program, while a weaker typing could only be withdrawn. What would unsettle the
cost argument is not a new type feature but a new *observation* — an operation
reading fields an open row does not name
([[invariants/fields-are-reached-by-name|fields-are-reached-by-name]]).

**Rejected: presence polymorphism** (Rémy `Pre`/`Abs`), which would type the
idiom. It is not additive here: flags pay off only on total, unordered rows,
where `[l: θ τ | ρ]` is the one slot for `l`. On a duplicate-retaining ordered
row, selecting `l` with `θ` unknown is stuck. Adopting it means replacing the
scoped-labels calculus with Rémy's — a new variable sort, the row half of
`unify.rs`, every row construction in `builtins.rs`, `generalize`, `fmt`, and
`Scheme` with its serde — to buy one idiom. **Rejected: a concatenation
constraint**, which has no most general unifier (`ρ ++ [l: Int]` against
`[l: α | ρ']` has two incomparable solutions) — Wand's result, not an accident of
this unifier ([[related/record-concatenation|record-concatenation]]) — so row
unification would stop being unitary and schemes would need a predicate slot:
the Gaster–Jones tax [[related/scoped-labels|scoped-labels]] says ral bought its
way out of, and one that a `Scheme` would carry into the persisted format
([[invariants/schemes-leave-closed|schemes-leave-closed]]).

## See also

[[design/row-types|row-types]] (the chain argument this generalises),
[[related/record-concatenation|record-concatenation]] (the published prices for
typing the operation ral declined),
[[invariants/fields-are-reached-by-name|fields-are-reached-by-name]] (what the
cost argument rests on),
[[related/scoped-labels|scoped-labels]] (Leijen has no concatenation either),
[[invariants/optionality-via-variants|optionality-via-variants]],
[[internals/type-inference|type-inference]], `docs/SPEC.md` §4.5.
