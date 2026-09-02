---
status: active
---

# Static diagnostics render at the seam, and never enter the registry

**A diagnostic that crosses the protocol as a sentence has already lost. The
caret, the code, the label and the hint are the diagnostic; the string is only
how it travels.**

## Decision

- `RunReport::into_report` renders **every** diagnostic, static and runtime
  alike. `Report::Static` carries `{ rendered, status }` — the whole report,
  prefix through hint — and the host prints it and exits on the status. The
  `Diagnostics { Parse, Types, Host }` wire enum is gone: no host classified on
  it, so it carried nothing but a chance to render three ways.
- `StaticDiagnostics::Parse`/`Types` carry the `Source` their carets point
  into. `Host` does not: it is spanless, and the invariant is type-level.
- **A failed compile registers nothing.** `compile_run` peeks a `FileId` so the
  program's spans carry this run's identity, and returns before
  `install_root_context` ever mints it. Since no program survives a static
  failure, no live span can reference its text — and `SourceDb` is append-only
  for the session's whole life precisely because live spans index it. Putting
  the text there would leave a slot that nothing can reach and nothing can
  reclaim, once per typo, forever.
- `format_static_diagnostics` is the single renderer, and settles the exit
  status alongside the text: 2 for a parse failure, 1 for a type failure, the
  host error's own otherwise. `Report::host_fault` shapes the engine's own
  refusals through it too.
- A **hook run registers nothing** either. Its program is an already-compiled
  value, so `run_framed` has no text; the entry it used to append named the
  empty string. That cost one permanent slot per prompt draw, and one per
  keystroke under a `buffer-change` hook.

## Why

The seam already said so. `into_report`'s own doc names it "the last point at
which the engine's `SourceDb` is in hand", promising the host "the full string
— prefix, status, hint, caret". Runtime errors kept that promise. Static ones
could not: they are produced before the source is registered, so at projection
time the registry was in hand but empty.

The regression came in with `ad916c82` (host-seam Phase 1). Routing the REPL
through the transport handed it `Report` instead of `RunReport`, so it no
longer held a `ParseError` or a `TypeError`, and the ariadne calls were dropped
rather than moved into core. What remained was `ParseError.message` — no span,
no `lex_kind` — and `TypeError`'s `Display`, which prints raw byte offsets and
drops `code()`, `render_label()` and `hint()`. Every host on the wire got a
sentence: the REPL, exarch, and so the model reading exarch's tool output.

Only two paths still looked right, and both by bypassing the wire: `batch.rs`
runs its own parse/elaborate/typecheck ahead of the run door, and the
`structural` REPL frontend re-typechecks locally before Enter. That the fix
restores the *same* phrasing those two already print is the point — one
renderer, so they cannot drift.

## Alternatives rejected

- **Register the source before compiling.** Closes the peek-then-register gap,
  and lets the static formatters read the db like the runtime one does. It also
  spends a permanent, unreachable registry slot on every failed line, and
  breaks the invariant `compile_run` reasons from — that nothing registers
  between the peek and `install_root_context`. The registry is not a cache; it
  is what live spans resolve through.
- **Ship the structure over the wire.** Serialising `ParseError`/`TypeError`
  plus the source text would let each host render for itself. It contradicts
  the standing rule that a live error cannot cross the protocol
  (`Ending::Raised` already carries `rendered`, not an `Error`), and it buys
  nothing: the one host that wants structure, the structural frontend, has it
  already by typechecking locally.
- **Restore the compact REPL parse-error heuristic.**
  `should_use_compact_parse_error` fired on a message that no longer exists
  anywhere in the tree; restoring it restores dead code. Every parse error
  renders with ariadne until a concrete error proves too noisy.
