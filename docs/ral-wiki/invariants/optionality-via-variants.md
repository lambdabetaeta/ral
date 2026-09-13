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
[[design/row-types|scoped labels]] and spread shadowing express defaults at the
level of data **when the records being merged are known where the merge is
written**. Over a record whose fields are not known there — a row-polymorphic
parameter — no merge can supply a default, because a spread wins on any field
it turns out to carry and nothing behind it could then be read
([[decisions/260913_an-open-spread-must-come-last|an-open-spread-must-come-last]]).
A field that may or may not be there is therefore a variant, like any other
absence.

This is a hard rule: do not add an `Option`/`Maybe` builtin or a null literal;
reach for an open variant.
