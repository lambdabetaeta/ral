---
status: rejected
---

# Related-work borrowables rejected

The [[related/index|related/]] reading of 2026-06-03 surfaced candidate
borrowings. One is proposed —
[[decisions/260603_stateful-handlers|stateful-handlers]] — and the following
are rejected, recorded here so they are not re-proposed without new grounds.

## Duplicate-label warning (Leijen §2.1)

A lint flagging duplicate labels in a closed record literal. Rejected: ral's
checker reports errors, not advisory warnings — a third diagnostic channel is
a new surface to maintain — and a duplicate label is *legal and meaningful*
under scoped labels, the same shadowing the spread idiom relies on; flagging
legal idioms teaches users to distrust the feature
([[design/row-types|row-types]], [[related/scoped-labels|scoped-labels]]).

## Static operation listing (`--effects`)

Listing a script's syntactically evident external heads before running — a
scope-based approximation of the *requirement half* that
[[related/system-c|system-c]] and
[[related/rows-and-handlers|rows-and-handlers]] compute with types. Rejected:
a computed head collapses the report to ⊤, so the listing is reliable exactly
when the script is trivial — an audit surface that fails open invites false
assurance. Authority questions are answered at run time by
[[design/grant|grant]] and post-hoc by [[design/audit|audit]].

## Record restriction `r − l` (Leijen)

The third Leijen primitive, from which update and rename each derive in one
line. Rejected: it re-admits un-shadowing at the data level; the
override-then-expose idiom is deliberately routed through `within` on the
dynamic context ([[related/scoped-labels|scoped-labels]],
[[design/scoping|scoping]]).

## Effect rows (Hillerström–Lindley)

Typing every arrow with an effect row. Rejected: ral's operations already mean
something — a handler shadows a meaning, never confers one — and the open
external-command namespace makes static rows noise. The refusal is argued in
full in [[related/rows-and-handlers|rows-and-handlers]].

## Capability boxing (System C)

`box`/`unbox` as a selective crossing from dynamic authority into capability
types. Rejected as a language feature: it contradicts the dynamic-authority
commitment of [[design/syscalls-are-effects|syscalls-are-effects]]. It remains
at most a research direction for the exarch boundary
([[related/system-c|system-c]]).
