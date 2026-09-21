# Optionality via variants

ral has no `Option` type and no null. Optionality — a value that may be present
or absent — is expressed as an **open variant**, idiomatically `` `some v `` and
`` `none ``, passed as an ordinary value and discriminated by `case`.

This follows from two other commitments:

- **Arguments are not where optionality lives**, because every invocable has
  [[invariants/fixed-arity|fixed arity]]: the caller always supplies every
  argument, and the *value* it supplies carries the presence or absence.
- **Absence is data, not a sentinel**: a null that inhabits every type
  reintroduces exactly the silent collapse the value/command
  [[design/cbpv|separation]] removes.

A variant makes the two cases distinct in the type and forces the consumer to
handle both.

Record fields give a second route to the same end, but only so far:
[[design/row-types|scoped labels]] and the put rule express defaults at the
level of data **when the records being merged are known where the merge is
written**. Over a record whose fields are not known there — a row-polymorphic
parameter — no merge can supply a default, and the reason is no longer that the
type cannot *say* a field may be missing. It can: a row's slot carries a
presence flag, and `l?: τ` reads "a `τ`, if it is there"
([[decisions/260921_a-field-is-a-flag-and-a-type|a-field-is-a-flag-and-a-type]]).
What there is no way to write is the *elimination*. **Presence has no
elimination form**: no rule branches on a flag and no term asks whether a field
is there, which is what keeps the flag parametric and makes forgetting an
absent field's payload lossless — no program could have read what the
traversals stop at. Presence exists to type a form's options and to give the
put rule a polymorphic type, not to be observed by ral code. A field that may or
may not be there is therefore still a variant, like any other absence.

This is a hard rule: do not add an `Option`/`Maybe` builtin or a null literal;
reach for an open variant.
