# related/ — comparison to existing work

ral's design read against the literature. Each page sits one published system (or
a tight cluster) beside ral's own pages and says three things: what
*corresponds*, what *deliberately diverges*, and what ral could *borrow*. How
these pages are written and kept honest is [[AGENTS|the maintainer contract]].

These pages are **durable on the external work** — a published calculus does not
change — and **keyed to ral's design** via each page's `against` stamp: a
`decisions/` page that supersedes a compared-against design page is the signal to
revisit.

## Pages

- [[related/system-c|system-c]] — Brachthäuser et al. 2022: effects and
  capabilities reconciled. The shared *box = thunk* identity, self-masking as
  capability-set subtraction, and the type-based pole of [[design/grant|grant]].
- [[related/scoped-labels|scoped-labels]] — Leijen 2005: the record calculus
  [[design/row-types|row-types]] implements, taken whole minus the restriction
  primitive; override is shadowing, never removal.
- [[related/handlers-of-algebraic-effects|handlers-of-algebraic-effects]] —
  Plotkin–Pretnar 2009: the founding handler calculus, on CBPV, with shell
  redirection as its own example; ral is its tail-resumptive fragment, and the
  pipe they could not express is ral's primitive.
- [[related/rows-and-handlers|rows-and-handlers]] — Hillerström–Lindley 2016:
  the effect typing ral declined — the same row machinery, extended to every
  arrow; nearly ral's runtime, the inverse of ral's wild/handleable split.
- [[related/call-by-push-value|call-by-push-value]] — Levy 1999/2003: the
  substrate taken as surface design; ral tags `F` with a payload route, adds the
  pipe as a combinator outside the calculus, and drops computation products.
