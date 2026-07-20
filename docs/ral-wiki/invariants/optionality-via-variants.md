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

Record fields give a second, structural route to the same end —
[[design/row-types|scoped labels]] and spread shadowing express defaults and
optional fields at the level of data.

This is a hard rule: do not add an `Option`/`Maybe` builtin or a null literal;
reach for an open variant.
