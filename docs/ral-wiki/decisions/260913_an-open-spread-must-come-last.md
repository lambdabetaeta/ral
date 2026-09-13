---
status: active
generated_at_commit: e7975e20
---

# An open spread must come last

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
then made `[:, ...$given, ...$dflt]` expressible and pointed the language's
whole defaults mechanism at it.

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
[T0023] a spread of a record whose fields aren't known here must come last
   Help: this spread wins on any field it happens to carry, and nothing here
   can say which, so 'host', 'port' could never be read — …
```

The mirror order stays legal and exact: `[:, ...[tag: 1], ...$g]` places the
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

**Two open spreads no longer unify.** `{|a b| [:, ...$a, ...$b]}` was inferred
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
