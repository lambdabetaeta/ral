# Fields are reached by name

**A record field is reached only by naming it in a row.** Selection names it,
a map pattern names it, and a builtin taking a record names its fields in the
builtin's own row ([[design/row-types|row-types]]). No operation reads a field
the type does not mention.

This is a rule about what may be *added*, not a description of a coincidence.
`keys`, `has`, and `fields` are typed over `Map<α>` rather than over a record
row (`core/src/typecheck/builtins.rs`), and that is load-bearing: a
`to-json :: [ρ] → F Str`, a record-typed `keys`, or any structural printer over
an open row would read fields no row names, and would thereby reopen
[[decisions/260913_an-open-spread-must-come-last|an-open-spread-must-come-last]] —
whose cost argument is that the entries behind an open spread are unreachable
under every sound typing. Such a builtin is a language decision, not a
convenience.

One operation *detects* what it cannot name: `equal` is `∀α β. α → β → Bool`
and compares a record's key–value pairs whole (`docs/SPEC.md` §4.8). Detection
is not reading — it yields no field's value, so the unreachability argument
stands — and it is what makes the weaker typing of a merge unsound rather than
merely useless: a closed row describing a value with unnamed extra fields is
observably wrong ([[related/record-concatenation|record-concatenation]]).

The rule is also why optionality cannot hide in a record's width: a field that
may or may not be there must be named to be read, and naming it in a result's
row forces every caller to supply it. Absence therefore travels as a variant
([[invariants/optionality-via-variants|optionality-via-variants]]).
