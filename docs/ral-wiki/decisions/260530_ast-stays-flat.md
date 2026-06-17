---
status: active
---

# The AST enum stays flat

The surface AST is a **single flat enum**, and it stays that way even as it
passes twenty variants. Grouping the variants into nested sub-enums "for
tidiness" is not an improvement here; it would:

- add an indirection layer that every match must thread through;
- obscure the one-to-one correspondence between grammar productions and
  variants;
- buy nothing the flat form lacks.

This is a deliberate standing choice, recorded so it is not "cleaned up" by a
future pass. Do not group the AST variants.

(Recorded 2026-05-30; the decision itself predates the wiki.)

See also [[map/core|core]], [[invariants/ir-pure-cbpv|ir-pure-cbpv]].
