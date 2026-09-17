---
status: active
generated_at_commit: a3ff030d
---

# Absence is merged, not defaulted

**A missing field is filled by merging in a record that supplies it, not by
attaching a fallback expression to the pattern that reads it.** Two changes
land as one decision: a bracketed literal's entries settle its kind, so a
two-record merge written with one explicit field
(`[tier: 'a', ...$given, ...$dflt]`) is a record and not a list; and
map-pattern defaults (`[port: p = 8080]`) are deleted, because with the merge
expressible a defaulted field is a second, worse mechanism for exactly what
spread shadowing already does at the level of data.

## Context

[[design/row-types|row-types]] already argued that scoped labels and spread
shadowing are how ral expresses optionality and defaults structurally — see
its "Scoped labels and spread shadowing are how ral expresses optionality and
defaults at the level of data" — but that route was unreachable for the one
case that matters here: a merge of two records. `[...$given, ...$dflt]`, with
nothing but spreads, has no entry to settle whether it is a record or a list,
and falls to list — silently, since a list of records is not a type error
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

**A bracketed literal's keys settle its kind, and one explicit field is enough
to settle it.** `[tier: 'a', ...$given, ...$dflt]` is a record merge: the
field says the literal is a record before either spread is read, and
record-spread's existing priority rule — an explicit entry beats every spread,
and of two spreads the first wins on a conflict — does the rest, giving
`$given` its fields back and falling through to `$dflt` for whatever it omits.
A literal of spreads alone stays a list; a merge that has no field of its own
to write is one the reader must anchor
([[design/records-and-maps|records-and-maps]]).

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
let [host: h, port: p] = [tier: 'a', ...$cfg, ...[port: 8080]]
```

An explicit entry always wins over a spread regardless of position, so the
fallback cannot be spelled as a bare `port: 8080` beside `$cfg`'s spread — it
would win unconditionally instead of only on absence. Spelling the default as
a second spread keeps it subject to the same first-spread-wins rule as
`$cfg`'s own fields.

This migration holds where `$cfg`'s own fields are known where the merge is
written. Where they are not — `$cfg` a row-polymorphic parameter — no spelling
of a default works, a map-pattern default no more than a second spread; that
limit is the row algebra's, not this decision's, and is recorded in
[[decisions/260913_an-open-spread-must-come-last|an-open-spread-must-come-last]].

### A soundness hole leaves with it

The removed implementation had a live gap worth recording, not as
justification but as a fact about what is gone: a defaulted field's inferred
type was unified with neither its default expression's type nor a present
field's actual type, so `let [flag: flag = 1] = [:]` typechecked and then
failed at runtime with a type mismatch, and an ill-typed default such as
`!{floor true}` escaped checking entirely — it was only ever forced if the
field was absent at run time, never checked against the field's inferred
type. Merging plain records has no such gap: every field in
`[tier: 'a', ...$given, ...$dflt]` is checked at the type the record literal
already gives it.

That last sentence held only for known records, and at the time it was written
a second hole hid the difference: a literal with two or more spreads discarded
both rows for a free row variable, so the merge appeared to typecheck over an
open `$given` too. Closing it showed that neither mechanism can default a field
of a record whose fields are unknown — so this decision's *replacement* claim
was too broad, while the deletion it argues for stands on its own.

## See also

[[design/row-types|row-types]] (the merge idiom this decision leans on),
[[invariants/fixed-arity|fixed-arity]] ("no variadic application and no
defaulted parameter" — a map-pattern default was the one place that did not
hold),
[[invariants/optionality-via-variants|optionality-via-variants]] (the other
half of "optionality is data, not a hole in a signature"),
`docs/SPEC.md` §4.5, §5.2, §14.
