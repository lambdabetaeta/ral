---
status: active
generated_at_commit: 584ed719
---

# A field is a flag and a type

**A record field is a presence flag paired with a type, the flag may be a
variable, and a field the row says is absent is absent *at the type its label
is assigned*.** Killing a field is therefore a constraint rather than a loss of
information, which is what makes row unification unitary; and a record literal
stops being a concatenation of its parts and becomes a **put** over one base.

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

**`Empty` means every label off the spine is absent *at the type the ambient
assignment gives that label*.** The assignment `Δ` maps a label to a type
variable `δ_l`, minted the first time that label is retired, and retiring a
field unifies its payload with `δ_l`:

```
Empty ~ (l : π·τ ; r)        π ~ Absent ;  τ ~ δ_l ;  Empty ~ r
```

That is what makes the algebra unitary. The solution that kills a field no
longer throws information away — `θ := Absent` retires the other side too, and
retiring it asks `String ~ δ_l` — so it is reachable from the eager one by
instantiation, and the eager answer is the mgu. The field rule may therefore
unify flags and payloads unconditionally: **no rule branches on a flag.**

**`Δ` is indexed by label rather than shared per row.** Rémy's closed tail
`∂(Absent·δ)` gives one `δ` to a whole row, which couples every absent payload
in it: an rc file writing only `surface:` would retire `bell` and
`recursion_limit` against one variable and demand `δ ~ Bool` and `δ ~ Int` at
once. Indexing by label dodges that because `Δ` is *generated*, not stored —
finite at every moment, total in effect — and every site that retires a field
has the label in hand.

**A record literal is a put over one base.** `[...b, l̄: v̄]` demands of `b` only
a *slot* at each written label — `Record(l̄: θ̄·β̄ ; ρ)`, flags and payloads
fresh, nothing imposed — and yields `Record(l̄: Present·τ̄ ; ρ)` over that same
tail. `[...[x: "old"], x: 1]` is now `[x: 1]` in the type as it always was at
run time, and a second base is refused where it is written.

**`Field::Absent` stays nullary.** A dead payload is recoverable from its
label, so no type stores one: `Δ`'s variables appear in no `Ty`, no `Row` and
no `Scheme`, are never quantified, printed or serialised, and
[[invariants/schemes-leave-closed|schemes-leave-closed]] is untouched.

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
  label may not be optional at two different ground types. Ordinary programs
  cannot reach it — a payload meets `δ_l` only when its flag dies, and the only
  construct putting a *ground* payload beside a *variable* flag is a declared
  option row — so it binds the declared tables and nothing else, and is checked
  where they register.
