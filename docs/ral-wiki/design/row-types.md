# Row-typed records

**ral records are open-row-polymorphic maps with scoped-label typing**
([[related/scoped-labels|Leijen 2005]]), over rows whose slots carry a
**presence flag** beside a type. `[host: String, port: Int]` has type
`[host: String, port: Int | ρ]`, where the tail variable `ρ` stands for unknown
further fields:

- **Field selection** unifies the target with `[label: α | ρ]` at `Present` and
  returns `α`.
- **Label mismatch** permutes labels past each other into a shared fresh tail
  (the Rémy 1989 rewrite).
- **Open rows compose.** A block `{ |opts| ... }` accepts
  `[host: String, port: Int | ρ]` and overwrites fields without knowing the
  tail, so record-passing composes.

**A slot is `Present τ`, `Absent`, or `θ·τ` — "a `τ`, if it is there"**
(`Field` in `core/src/typecheck/ty.rs`), the flag being a variable in a
two-point union-find. A slot prints `l: τ`, `l?: τ`, or nothing at all.
Several consequences follow:

- **`Empty` says something.** It means *every label off the spine is absent, at
  the type the ambient assignment gives that label* — the assignment `Δ` maps a
  label to `δ_l`, minted the first time that label is retired, and retiring a
  field unifies its payload with it. That is what makes the algebra unitary:
  the solution that kills a field constrains `δ_l` rather than discarding
  information, so it is an instance of the eager one and the eager one is the
  most general unifier
  ([[decisions/260921_a-field-is-a-flag-and-a-type|a-field-is-a-flag-and-a-type]]).
- **`Absent` stores nothing.** A dead payload is recoverable from its label, so
  `Δ`'s variables appear in no type, row or scheme — never quantified, never
  printed, never serialised.
- **Absence before `Empty` is an equation** — `(l: Absent ; Empty) = Empty` —
  and absence before a *variable* tail is not: exclusion is an invariant of row
  introduction here rather than data on the variable, as it is in Rémy.
- **Two label alphabets, one per type former** — `Label::Field` for `Record`,
  `Label::Case` for `Variant` (`core/src/typecheck/ty.rs`) — and the two never
  unify. The alphabet is a constructor, not a character: a field named
  `` '`dev' `` is `Field("`dev")` and no spelling of it reaches
  `Case("dev")`. `unify_row` refuses a row whose spine carries both, which is
  what stops a record row and a variant row meeting through a shared tail; the
  backtick is added when a label is shown.
- **Structural equality of closed records is order-insensitive:**
  `[a: 1, b: 2] == [b: 2, a: 1]`.

**A record literal is a put over one base, not a concatenation of parts.**
`[...b, l̄: v̄]` demands of `b` only a *slot* at each written label —
`Record(l̄: θ̄·β̄ ; ρ)` with flags, payloads and tail fresh, nothing imposed on
any of them — and yields `Record(l̄: Present·τ̄ ; ρ)` over that same tail. So an
explicit entry beats the base at its label whatever the base holds there and
wherever in the bracket it sits, position never being in question; and the
result is *flat*, `[...[x: "old"], x: 1]` being `[x: 1]` in the type as it
always was at run time.

**One base is the restriction the put rule carries.** Two bases would be a
merge, and which of two unknown remainders wins is a question a literal cannot
answer — selecting on how much the store happens to know is exactly how a
verdict comes to depend on where a statement was written. So `[...$a, ...$b,
z: true]` is refused where it is written, against the bracket's *final*
classification: a computed key makes the bracket a map, whose spreads are
entries and never bases, and a list's spreads never were. What a presence flag
does **not** buy back is the merge: "the base's value here, or this default if
it lacks one" over a record whose fields are not known here is still not
sayable, because no rule branches on a flag. Over an unknown record, absence
travels as a variant
([[invariants/optionality-via-variants|optionality-via-variants]]); a record
that exists to be merged should be a block, where the merge is application.

**A `case` closes a variant row, and the syntax is what lets it.** The arms are
written out at the `case`, so the label set is known when the rule fires: the
scrutinee's row unifies with exactly that set, and coverage is decided there,
always ([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).
An *open* scrutinee row absorbs an arm label it has not been seen to construct
— `` let v = `ok 5 `` then a `case` with an `` `err `` arm typechecks — which is
principal row inference and not a gap in the proof: the row records what the
program has shown, and the `case` is one more such showing.

**Variants share the machinery and acquire nothing.** Every flag a rule builds
on a variant row is `Present`; the only way one acquires `Absent` is a peel,
and a peel against a `Present` tag is the error `case` and injection want. So
no variant row holds a resolved `Absent` and the retirement rule is a no-op
there — but a variant shares the occurs check, the recursion guards and the key
machinery with records, and a variant's payload may itself be a record with an
optional field.

Scoped labels and the put rule are how ral expresses defaults at the level of
data rather than argument lists, wherever the records in hand are known; where
they are not, optionality is a variant's job — see
[[invariants/optionality-via-variants|optionality-via-variants]],
[[invariants/fields-are-reached-by-name|fields-are-reached-by-name]] and [[invariants/fixed-arity|fixed-arity]].
Rows type data only, never effects — that refusal is argued against the
literature in [[related/rows-and-handlers|rows-and-handlers]].

**Realised in** [[internals/type-inference|type-inference]] (row unification by the Rémy rewrite).

Cite: RATIONALE §"Structured values cross once"; `docs/SPEC.md` §4, §4.5.
