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

Scoped labels and spread shadowing are how ral expresses optionality and
defaults at the level of data rather than argument lists — see
[[invariants/optionality-via-variants|optionality-via-variants]] and [[invariants/fixed-arity|fixed-arity]].
Rows type data only, never effects — that refusal is argued against the
literature in [[related/rows-and-handlers|rows-and-handlers]].

**Realised in** [[internals/type-inference|type-inference]] (row unification by the Rémy rewrite).

Cite: RATIONALE §"Record types and scoped labels"; `docs/SPEC.md` §2, §6.
