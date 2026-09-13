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
an unknown remainder. The only sound alternative is to bind that row to the
fields behind it — which forces every caller to supply them, and so proves
those entries dead. Refusing therefore loses no program that could have run,
and the error can name the fields that would have been unreachable:

```
[T0023] a spread of a record whose fields aren't known here must come last
   Help: this spread wins on any field it happens to carry, and nothing here
   can say which, so 'host', 'port' could never be read — …
```

The mirror order stays legal and exact: `[:, ...[tag: 1], ...$g]` places the
known chain first and lets `$g` be the open tail.

**A label written twice in one literal is refused** (`T0022`). The runtime was
last-wins there and first-wins everywhere else in the same literal; rather than
carry one exception, position now means one thing throughout. Computed keys
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

**Rejected: presence polymorphism** (Rémy `Pre`/`Abs`), which would type the
idiom. It is not additive here: flags pay off only on total, unordered rows,
where `[l: θ τ | ρ]` is the one slot for `l`. On a duplicate-retaining ordered
row, selecting `l` with `θ` unknown is stuck. Adopting it means replacing the
scoped-labels calculus with Rémy's — a new variable sort, the row half of
`unify.rs`, every row construction in `builtins.rs`, `generalize`, `fmt`, and
`Scheme` with its serde — to buy one idiom. **Rejected: a concatenation
constraint**, which has no most general unifier (`ρ ++ [l: Int]` against
`[l: α | ρ']` has two incomparable solutions), so row unification would stop
being unitary and schemes would need a predicate slot — the Gaster–Jones tax
[[related/scoped-labels|scoped-labels]] says ral bought its way out of.

## See also

[[design/row-types|row-types]] (the chain argument this generalises),
[[related/scoped-labels|scoped-labels]] (Leijen has no concatenation either),
[[invariants/optionality-via-variants|optionality-via-variants]],
[[internals/type-inference|type-inference]], `docs/SPEC.md` §4.5.
