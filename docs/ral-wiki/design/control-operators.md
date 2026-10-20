# Control flow is library; seven operators are syntax

**ral's grammar reserves syntax for exactly seven control operators** —
`within`, `grant`, `try`, `guard`, `audit`, `spawn`, `all` — alongside the
syntactic forms `if`, `case`, and `?`. Everything else a shell usually bakes in (`for`, `while`, `map`,
`each`, `retry`) is an ordinary prelude function taking thunks.

An operator earns grammar only when both conditions hold:

1. its typing rule is not derivable from ordinary Hindley–Milner;
2. its semantics cannot be expressed as a function over thunks without lying
   about the signature.

The seven qualify because each manipulates dynamic frames, audit ownership, or
the ownership of concurrent work that live in `Shell` state, not in any value
the body can observe:

- `try` unifies body and handler output and threads an error record;
- `guard` threads cleanup;
- `within` / `grant` / `audit` scope handler frames, capability frames, and
  audit ownership over the body's whole dynamic extent, persisting across tail
  calls;
- `spawn` runs its body on a worker owned by the session or by the enclosing
  worker, and its title is optional — an operand only syntax may leave out,
  since no value's signature spells an optional argument
  ([[invariants/fixed-arity|fixed-arity]]);
- `all` runs its labelled bodies on workers the expression owns and types the
  record of their values field by field, each body demanded as a value, a
  rule no function over a record of thunks has
  ([[decisions/261019_await-is-the-block-run-here|await-is-the-block-run-here]]).

`if`, `case`, `?` are in the grammar for ergonomics — they take arbitrarily many
arms — but their typing is ordinary. What `?` and `try` *do* — the failure model
they serve — is on [[design/failure|failure]].

Reserved status keeps these names stable and grep-safe and carries them in the
IR as dedicated nodes rather than string-keyed builtins. An operator lacking
both criteria stays in the prelude: a user-defined `retry` requires no parser
growth.

Audit ownership is one such mechanism: each scope node owns the trail its body
produces, while process boundaries only *transport* fragments — the full
account of lexical ownership lives in [[design/audit|audit]]. `audit` is the
one of the seven that hands the ownership back as a value, a report pairing the
body's outcome with that trail; the others own a trail without ever naming
one.

See also [[design/failure|failure]], [[design/scoping|scoping]], [[design/effects-handlers|effects-handlers]], [[design/grant|grant]].
Cite: RATIONALE §"The grammar is the residue", §"Failure is not truth";
`docs/SPEC.md` §3.6, §8.
