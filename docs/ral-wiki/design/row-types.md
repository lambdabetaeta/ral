# Row-typed records

**ral records are open-row-polymorphic maps with scoped-label typing**
([[related/scoped-labels|Leijen 2005]]), over plain Rémy rows: a slot is a
label and a type, nothing else. `[host: String, port: Int]` has type
`[host: String, port: Int | ρ]`, where the tail variable `ρ` stands for unknown
further fields:

- **Field selection** unifies the target with `[label: α | ρ]` and returns `α`.
- **Label mismatch** permutes labels past each other into a shared fresh tail
  (the Rémy 1989 rewrite).
- **Open rows compose.** A block `{ |opts| ... }` accepts
  `[host: String, port: Int | ρ]` and replaces fields without knowing the
  tail, so record-passing composes.

**`Empty` says something.** It means *every label off the spine is absent*, so
a field met against it is an error — *no field `l` in this record*, or *this
record needs `l`*, by which side lacks it. There is no slot that may be
missing: an optional field is a variant's job
([[invariants/optionality-via-variants|optionality-via-variants]]), so the row
algebra stays textbook and order-independence is a fact about the term rules
alone ([[decisions/260930_a-table-never-enters-the-unifier|a-table-never-enters-the-unifier]]).

- **Two label alphabets, one per type former** — `Label::Field` for `Record`,
  `Label::Case` for `Variant` (`core/src/typecheck/ty.rs`) — and the two never
  unify. The alphabet is a constructor, not a character: a field named
  `` '`dev' `` is `Field("`dev")` and no spelling of it reaches
  `Case("dev")`. `unify_row` refuses a row whose spine carries both, which is
  what stops a record row and a variant row meeting through a shared tail; the
  backtick is added when a label is shown.
- **Structural equality of closed records is order-insensitive:**
  `[a: 1, b: 2] == [b: 2, a: 1]`.

**A record literal with a spread is an update of one base.** `[...b, l̄: v̄]`
demands of `b` a slot at each written label — `Record(l̄: β̄ ; ρ)` with payloads
and tail fresh, nothing imposed on the payloads — and yields
`Record(l̄: τ̄ ; ρ)` over that same tail. A field may change its type, and the
result is *flat*: `[...[x: "old"], x: 1]` is `[x: 1]` in the type as it always
was at run time. A spread never adds a field: naming one the base lacks is the
error *`$b` has no field `l` to update*, which offers the nearest spelling or
else the three idioms — a field the record has from the start, a nested record,
a map. Elm's `{ r | k = v }` is the whole feature.

**The spread comes first, and there is one.** A spread after a field, and a
second spread, are parse errors against the bracket's *final* classification: a
computed key makes the bracket a map, whose spreads are entries and never
bases, and a list's spreads never were. Two bases would be a merge, and which
of two unknown remainders wins is a question a literal cannot answer —
selecting on how much the store happens to know is exactly how a verdict comes
to depend on where a statement was written. "The base's value here, or this
default if it lacks one" over a record whose fields are not known here is not
sayable; over an unknown record, absence travels as a variant, and a record
that exists to be merged should be a block, where the merge is application.

**A `case` closes a variant row, and the syntax is what lets it.** The arms are
written out at the `case`, so the label set is known when the rule fires: the
scrutinee's row unifies with exactly that set, and coverage is decided there,
always ([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).
An *open* scrutinee row absorbs an arm label it has not been seen to construct
— `` let v = `ok 5 `` then a `case` with an `` `err `` arm typechecks — which is
principal row inference and not a gap in the proof: the row records what the
program has shown, and the `case` is one more such showing.

**Variants share the machinery.** A variant shares the occurs check, the
recursion guards and the key machinery with records, and a variant's payload
may itself be a record.

Scoped labels and the update rule are how ral expresses defaults at the level
of data rather than argument lists, wherever the records in hand are known;
where they are not, optionality is a variant's job — see
[[invariants/optionality-via-variants|optionality-via-variants]],
[[invariants/fields-are-reached-by-name|fields-are-reached-by-name]] and [[invariants/fixed-arity|fixed-arity]].
Rows type data only, never effects — that refusal is argued against the
literature in [[related/rows-and-handlers|rows-and-handlers]].

**Realised in** [[internals/type-inference|type-inference]] (row unification by the Rémy rewrite).

Cite: RATIONALE §"Structured values cross once"; `docs/SPEC.md` §4, §4.5.
