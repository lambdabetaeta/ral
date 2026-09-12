---
status: active
generated_at_commit: a3ff030d
---

# Absence is merged, not defaulted

**A missing field is filled by merging in a record that supplies it, not by
attaching a fallback expression to the pattern that reads it.** Two changes
land as one decision: a bracketed literal now classifies as a record when it
opens with `:` or contains a `key: value` entry, which makes a pure
two-record merge (`[:, ...$given, ...$dflt]`) expressible for the first time;
and map-pattern defaults (`[port: p = 8080]`) are deleted, because with the
merge expressible a defaulted field is a second, worse mechanism for exactly
what spread shadowing already does at the level of data.

## Context

[[design/row-types|row-types]] already argued that scoped labels and spread
shadowing are how ral expresses optionality and defaults structurally — see
its "Scoped labels and spread shadowing are how ral expresses optionality and
defaults at the level of data" — but that route was unreachable for the one
case that matters here: a *pure* merge of two records, with no field of its
own to anchor the literal's shape. `[...$given, ...$dflt]`, with nothing but
spreads, had no `key: value` entry to settle whether it was a record or a
list, and fell to list — silently, since a list of maps is not a type error
until something indexes it. Reaching for a default *inside the pattern*
instead was the only way to say "fall back to this value" without that
literal.

A map-pattern default was also the one place pattern matching could cause an
effect. `IrPattern`'s only `Comp` was a defaulted field's fallback
expression, and that single fact was the reason several traversals existed
at all: `elaborator.rs` had to elaborate a pattern *before* its own names
entered scope so a default resolved outward rather than to itself;
`ir::walk_pattern_defaults` was a dedicated free-name walk over patterns,
otherwise unwalked because a pattern's own names are bound, never
referenced; `typecheck::annotate::annotate_pattern` existed only to carry
the checker's verdict onto a default's `Comp`, a pure clone everywhere else;
and `typecheck::infer::bind_pattern`'s `Map` arm shaped the inferred row
around which entries had a default and which did not. It also sat uneasily
beside [[invariants/fixed-arity|fixed-arity]]'s "no variadic application and
no defaulted parameter" — a defaulted *pattern field* was the one place that
rule did not reach.

## Decision

**A bracketed literal is a record if it opens with `:` or contains a
`key: value` entry; otherwise it is a list.** `[:]` is that marker's
degenerate case rather than a special-cased empty-map token — nothing
follows the `:`. This is what makes `[:, ...$given, ...$dflt]` a record
merge: the leading `:` settles the literal's shape before either spread is
read, and record-spread's existing priority rule — first spread wins on a
conflict — does the rest, giving `$given` its fields back and falling
through to `$dflt` for whatever it omits.

**Map-pattern defaults are deleted.** `MapPatternEntry` loses its `default`
field; the parser no longer reads an `=` after a map-pattern entry's
sub-pattern; every traversal that existed only to reach a default is deleted
outright rather than left running over an always-`None` field:
`elab_pattern`'s default arm, `ir::walk_pattern_defaults` and its four call
sites, `annotate_pattern` (now a plain clone at each of its former call
sites), and `free_refs::collect_default_free_refs`. `bind_pattern`'s `Map`
arm in the checker now extends the inferred row with every entry
unconditionally — a pattern field is always required, exactly as
[[invariants/fixed-arity|fixed-arity]] already requires of an argument.
`evaluator::pattern::stage_pattern` no longer runs a nested machine to
evaluate a fallback, so it sheds the `mooring` and `shell` parameters it held
only for that; `bind_pattern`/`bind_pattern_staged` still take `shell` (their
`observe` callback reaches it independently of pattern matching) but drop
`mooring`, which nothing in the pattern module needs any more. A pattern
absent a field is a plain runtime error, `key '…' not found`, exactly as it
already was for a required field.

### Migration

```ral
let [host: h, port: p = 8080] = $cfg
```

becomes

```ral
let [host: h, port: p] = [:, ...$cfg, ...[port: 8080]]
```

An explicit entry always wins over a spread regardless of position, so the
fallback cannot be spelled as a bare `port: 8080` beside `$cfg`'s spread — it
would win unconditionally instead of only on absence. Spelling the default as
a second spread keeps it subject to the same first-spread-wins rule as
`$cfg`'s own fields.

### A soundness hole leaves with it

The removed implementation had a live gap worth recording, not as
justification but as a fact about what is gone: a defaulted field's inferred
type was unified with neither its default expression's type nor a present
field's actual type, so `let [flag: flag = 1] = [:]` typechecked and then
failed at runtime with a type mismatch, and an ill-typed default such as
`!{floor true}` escaped checking entirely — it was only ever forced if the
field was absent at run time, never checked against the field's inferred
type. Merging plain records has no such gap: every field in `[:, ...$given,
...$dflt]` is checked at the type the record literal already gives it.

## See also

[[design/row-types|row-types]] (the merge idiom this decision leans on),
[[invariants/fixed-arity|fixed-arity]] ("no variadic application and no
defaulted parameter" — a map-pattern default was the one place that did not
hold),
[[invariants/optionality-via-variants|optionality-via-variants]] (the other
half of "optionality is data, not a hole in a signature"),
`docs/SPEC.md` §4.5, §5.2, §14.
