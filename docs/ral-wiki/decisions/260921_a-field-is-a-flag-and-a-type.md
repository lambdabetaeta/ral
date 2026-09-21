---
status: active
generated_at_commit: f06a5056
---

# A field is a flag and a type

**A record field is a presence flag paired with a type, and the flag may be a
variable.** A form's options are then sayable in a type, so `within $o` has a
verdict; and a record literal stops being a concatenation of its parts and
becomes a **put** over one base.

The assignment `Δ` this decision introduced, which made an absent field absent
*at the type its label is assigned*, was withdrawn four commits later:
[[decisions/260921_unitarity-lives-in-the-term-rules|unitarity-lives-in-the-term-rules]].
The paragraphs below are written as they now stand.

## Context

`Row::Extend(label, type, rest)` said only *this label, at this type*. Absence
was the absence of an entry, so `Row::Empty` said *nothing further is here* and
said nothing about what was not here. Three things followed from that silence.

**The literal rule was a concatenation, and one that could not be written.** A
literal's row was the chain of its parts in precedence order. Two chains
compose only while the first ends in `Empty`; if it ends in `Var(ρ)` there is
no position after the remainder, so `[x: 0, ...$g, ...$dflt]` was refused
(`T0023`), and `[...[x: "old"], x: 1]` kept *both* entries and then read as a
record with two `x` fields — a spine the evaluator, which keeps one, never
builds.

**`within $o` had no verdict.** A form's options were checked by walking a
*literal*'s entries; a bound bundle went through the other arm, which pinned
nothing. So `within [dir: 42]` was refused and `let o = [dir: 42]; within $o`
was accepted and died at run time. Giving the bound spelling the literal's
verdict needs the option set to be sayable *in a type*, and "this field may be
missing" was not sayable at all.

**The obvious repair has no most general unifier.** With an `Empty` that
carries nothing, `[x: θ·α]` against `[x: θ'·String]` has two incomparable
solutions: `θ := Absent`, leaving `α` free, and `α := String`, leaving the
flags open. Neither is more general, so an eager algorithm guesses and the
verdict depends on the order the equations arrive in — which is the disease
this line of work started from.

## Decision

**A field is `Present τ`, `Absent`, or `Var(θ, τ)`** — "a `τ`, if it is
there" — over a two-point flag store beside the four the unifier already had.

**`Empty` means every label off the spine is absent**, full stop:

```
Empty ~ (l : π·τ ; r)        π ~ Absent ;  Empty ~ r
```

The field rule unifies flags, then payloads when both sides have one:
**no rule branches on a flag.** The eager answer is not the mgu of the deciding
equation on its own — that is what `Δ` bought and what its withdrawal gives
back — but it is order-independent on the terms the rules can build, which is
the claim that was actually wanted. See
[[decisions/260921_unitarity-lives-in-the-term-rules|unitarity-lives-in-the-term-rules]].

**A record literal is a put over one base.** `[...b, l̄: v̄]` demands of `b` only
a *slot* at each written label — `Record(l̄: θ̄·β̄ ; ρ)`, flags and payloads
fresh, nothing imposed — and yields `Record(l̄: Present·τ̄ ; ρ)` over that same
tail. `[...[x: "old"], x: 1]` is now `[x: 1]` in the type as it always was at
run time, and a second base is refused where it is written.

**`Field::Absent` stays nullary.** Nothing can read under an absent flag, so
there is nothing beside an absent field to read, key, quantify or
occurs-check, and no traversal has to invent a payload where the row says there
is none. [[invariants/schemes-leave-closed|schemes-leave-closed]] is untouched.

## Consequences

- **`T0023` and the open-spread rule go**, and with them the two-spread merge:
  [[decisions/260913_an-open-spread-must-come-last|an-open-spread-must-come-last]]
  is superseded. One base is a real restriction and withdraws nothing in the
  corpus — every two-spread bracket in the tree is a list or a map.
- **A repeated label in a record pattern is refused at parse**, beside the
  repeated `case` arm: it needs no types to see, and it used to arrive as a
  bogus complaint about the value being matched.
- **Presence has no elimination form.** No term asks whether a field is there;
  `has`, `get`, `keys` and `union` are map operations. That is what keeps `θ`
  parametric and the erasure lossless — no program could have read what the
  traversals stop at — and it is why
  [[invariants/optionality-via-variants|optionality-via-variants]] keeps its
  rule while its reason changes: presence exists to type a form's options, not
  to be observed by ral code.
- **A field's presence resolves exactly as a row's spine does.**
  `resolve_field` is `resolve_row`'s twin and is called from the same places —
  `apply_row`, `free_row`, the occurs check, and `row_key`, which must be
  presence-aware or the co-inductive guard stops recognising one obligation
  across a flag resolving.
- **Two soundness obligations are carried, not discharged**: that forgetting a
  payload behind an absent field lets no program go wrong — conditional on the
  decoder and module boundaries, which hand out values no rule has checked —
  and the recursive case, where the admissible-graph policy is *chosen* (occurs
  descends through payloads) and the termination proof stays owed. Both are
  written down in [[internals/type-inference|type-inference]]; neither is
  claimed as a theorem.
- **One condition is owed and is a rule, not a result**: within one check, a
  label may not be optional at two types that will not unify. Ordinary programs
  cannot reach it — the only construct putting a *ground* payload beside a
  *variable* flag is a declared option row — so it binds the declared tables
  and nothing else, and is checked where they register. With `Δ` withdrawn this
  condition is what carries order-independence, so it is load-bearing rather
  than incidental.

## A correction, made while implementing

The plan this decision came from claimed that two forms meeting in one list
narrow *both* to the empty bundle, and asked for a diagnostic naming which two
forms met. Neither holds, and the reason is this design's own.

**Both forms stay usable at their own options.** In

```ral
let w = { |o| within $o { () } }
let g = { |o| grant $o { () } }
return [$w, $g]
```

each wrapper's presence flags are let-generalised, so the list merges two
*fresh* instances and leaves `w` and `g` themselves alone: `w [dir: '/tmp']`
still checks after the list, and so does `g [net: true]`. What the merge
narrows is the merged value. `$both[0]` applied to `[dir: …]` is refused,
naming the option the all-absent row no longer has, which is what
`two_forms_in_one_list_narrow_to_the_empty_bundle` pins.

**And no message can say which two forms met.** A row carries labels, fields
and a tail; a flag carries a two-point value; neither carries provenance, and
no side table records which form asked for a label. Naming the two
forms would mean adding provenance to a row or a flag, which is a second kind
of data on the type and a cost this design declines to pay for a diagnostic.
The refusal names the missing option instead, which is the fact the program
tripped on.
