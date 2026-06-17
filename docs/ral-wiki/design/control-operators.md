# Control flow is library; five operators are syntax

**ral's grammar reserves syntax for exactly five control operators** — `within`,
`grant`, `try`, `guard`, `audit` — alongside the syntactic forms `if`, `case`,
and `?`. Everything else a shell usually bakes in (`for`, `while`, `map`,
`each`, `retry`) is an ordinary prelude function taking thunks.

An operator earns grammar only when both conditions hold:

1. its typing rule is not derivable from ordinary Hindley–Milner;
2. its semantics cannot be expressed as a function over thunks without lying
   about the signature.

The five qualify because each manipulates dynamic frames or audit ownership that
live in `Shell` state, not in any value the body can observe:

- `try` unifies body and handler output and threads an error record;
- `guard` threads cleanup;
- `within` / `grant` / `audit` scope handler frames, capability frames, and
  audit ownership over the body's whole dynamic extent, persisting across tail
  calls.

`if`, `case`, `?` are in the grammar for ergonomics — they take arbitrarily many
arms — but their typing is ordinary. What `?` and `try` *do* — the failure model
they serve — is on [[design/failure|failure]].

Reserved status keeps these names stable and grep-safe and carries them in the
IR as dedicated nodes rather than string-keyed builtins. An operator lacking
both criteria stays in the prelude: a user-defined `retry` requires no parser
growth.

Audit ownership is one such mechanism: each scope node owns its body's audit
nodes, while process boundaries only *transport* fragments — the full account of
lexical ownership lives in [[design/audit|audit]].

See also [[design/failure|failure]], [[design/scoping|scoping]], [[design/effects-handlers|effects-handlers]], [[design/grant|grant]].
Cite: RATIONALE §"Control flow is library", §"Audit is one mechanism";
`docs/SPEC.md` §1, §10.
