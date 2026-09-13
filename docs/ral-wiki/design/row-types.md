# Row-typed records

**ral records are open-row-polymorphic maps with scoped-label typing**
([[related/scoped-labels|Leijen 2005]]). `[host: String, port: Int]` has type `[host: String, port: Int | ρ]`,
where the tail variable `ρ` stands for unknown further fields:

- **Field selection** unifies the target with `[label: α | ρ]` and returns `α`.
- **Label mismatch** permutes labels past each other into a shared fresh tail
  (the Rémy 1989 rewrite).
- **Open rows compose.** A block `{ |opts| ... }` accepts
  `[host: String, port: Int | ρ]` and adds or overrides fields without knowing
  the tail, so record-passing composes.

**Duplicate labels are permitted; selection takes the first.** Several
consequences follow:

- **A spread prepends and thus shadows.** `[...$base, port: 9090]` over
  `$base : [host: String | ρ]` infers `[port: Int, host: String | ρ]`.
- **No restriction operator (`Pre` / `Abs`) is needed.** The rewrite already
  preserves the relative order of same-label entries, so shadowing is coherent.
- **Records and tags draw on two label alphabets** — backtick-prefixed for
  variants and tag-keyed records, bare for ordinary records — and the two never
  unify.
- **Structural equality of closed records is order-insensitive:**
  `[a: 1, b: 2] == [b: 2, a: 1]`.

**Explicit beats spread regardless of position, because a row is a chain and
only one end of it is open.** A row is `Extend(label, type, rest)` ending in
`Empty` (closed) or `Var(ρ)` (open, the unknown remainder), and
selection-takes-first means "X beats Y" is "X sits earlier in the chain." An
explicit entry can always be prepended onto a spread's chain, whatever that
chain holds and however it ends — so the rule holds regardless of position,
because position is never in question. The reverse would need the spread's
fields to come first, i.e. the explicit entry appended after the spread's
chain: fine if that chain ends in `Empty`, impossible if it ends in `Var(ρ)`,
since there is no position after the remainder to append into. Making the
reverse work for open rows needs a presence flag per field (Rémy-style
`Pre`/`Abs`) — exactly the restriction operator the bullet above says ral does
not have.

**The same argument binds spread against spread, and that is what the missing
flag costs.** `[...$cfg, ...[port: 8080]]` prepends one whole chain onto
another, so a duplicate label survives and selection-takes-first resolves it —
but only while `$cfg`'s chain ends in `Empty`. If it ends in `Var(ρ)` there is
again no position after the remainder, and a spread that wins on any field it
turns out to carry would make the defaults behind it unreadable, so the literal
is refused
([[decisions/260913_an-open-spread-must-come-last|an-open-spread-must-come-last]]).
So a `Pre`/`Abs` flag buys exactly one thing scoped labels cannot: "the
spread's value here, or this default if it lacks one" over a record whose
fields are *not* known here. ral declines to buy it — over an unknown record,
absence travels as a variant
([[invariants/optionality-via-variants|optionality-via-variants]]).

**A `case` closes a variant row, and the syntax is what lets it.** The arms are
written out at the `case`, so the label set is known when the rule fires: the
scrutinee's row unifies with exactly that set, and coverage is decided there,
always ([[decisions/260811_case-is-syntax-try-is-not|case-is-syntax-try-is-not]]).
An *open* scrutinee row absorbs an arm label it has not been seen to construct
— `` let v = `ok 5 `` then a `case` with an `` `err `` arm typechecks — which is
principal row inference and not a gap in the proof: the row records what the
program has shown, and the `case` is one more such showing.

Scoped labels and spread shadowing are how ral expresses defaults at the level
of data rather than argument lists, wherever the records in hand are known;
where they are not, optionality is a variant's job — see
[[invariants/optionality-via-variants|optionality-via-variants]] and [[invariants/fixed-arity|fixed-arity]].
Rows type data only, never effects — that refusal is argued against the
literature in [[related/rows-and-handlers|rows-and-handlers]].

**Realised in** [[internals/type-inference|type-inference]] (row unification by the Rémy rewrite).

Cite: RATIONALE §"Structured values cross once"; `docs/SPEC.md` §4, §4.5.
