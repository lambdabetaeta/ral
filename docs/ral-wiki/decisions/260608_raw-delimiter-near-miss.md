---
status: proposed
---

# A `#\'` near-miss errors in value position and back-points the quote it strands elsewhere

The raw-string delimiter is a *run of `#` immediately followed by `'`* — `#'`,
`##'`, `###'` — and a `#`-run **not** followed by `'` opens a comment to end of
line. The mistype `#\'` therefore falls in a blind spot: the backslash breaks the
delimiter, the run opens a comment, and the rest of the line is silently swallowed.
**The failure then surfaces far from its cause** — a later lone `'` raises [L0001]
*unterminated*, anchored at the close, while the stray backslash at the open goes
unmentioned. This is the trap documented in the Strings section of
`exarch/data/ral.md` (`echo #\'…\'# > f` "writes nothing and exits 0"); it costs a
reader many turns chasing the wrong line.

The fix is **diagnostic, not grammatical**: `#'` still opens a raw string, a bare
`#`-run still opens a comment, and `#\'` is still *not* an opener (raw strings
escape nothing — there is no escape to honour). What changes is that the lexer no
longer lets the near-miss pass in silence.

## Position is the signal of intent, so the diagnostic splits on it

A `#`-run reaches the lexer by one of two paths, and the path discloses what the
author meant:

- **Value position** — the run is dispatched by `next_token`'s `#` arm, reached
  after inline whitespace or at start of input (`echo #\'…`, the canonical
  reproduction). Here a raw string is the natural reading, so a near-miss is **a
  hard lex error**, [L0004], anchored at the `#\'` span and naming the real
  delimiter.
- **Line-leading position** — the run is consumed by `scan_separator`'s comment
  skip, immediately after a statement separator. Here a `#` reads naturally as a
  comment, so the run **stays a comment** — but its span is recorded, and if a
  quote later dangles, [L0001]'s secondary label points back at it as the likely
  cause.

The asymmetry is deliberate. Rejecting a line-leading `#\'` would change the
grammar of comments; leaving a value-position `#\'` silent is exactly the trap.
Each path gets the treatment its position warrants.

## The mechanism: one new error kind, one near-miss memory

- *`LexErrorKind::RawDelimiterEscaped { hashes, quote, span }`* → **[L0004]**,
  joining the structured lexer codes [L0001]–[L0003]. Its message is run-length
  aware (``raw-string delimiter is `##'`, not `##\'` `` for a two-hash run); the
  primary label reads *remove the backslash*, with the rule in the hint.
- *`UnterminatedString`* gains `hash_escape: Option<Span>`, filled from a lexer
  field that records the most recent line-leading near-miss. The field is
  **cleared on every successful string close**, so a stale near-miss cannot haunt
  an unrelated later dangling quote — the heuristic fires only when no complete
  string intervened between the near-miss and the open quote.
- Rendering ([[map/core/diagnostics|diagnostics]]) repurposes
  `UnterminatedString`'s single secondary-label slot: where today it points at EOF
  (*expected closing `'` here*), a recorded near-miss redirects it to the `#\'`
  span (*this `#` opened a comment, not a raw string*), plus an explanatory hint.

Both detections share one predicate — *a `#`-run of length n followed by `\'` or
`\"`* — so value-position rejection and line-leading recording cannot drift apart.

## Consequences

- `echo #\'…\'# > f` changes from a silent no-op (exit 0) to a hard [L0004]
  error. This is the one **observable behaviour change**, confined to value
  position; the Strings section of `exarch/data/ral.md` is rewritten to describe
  the rejection rather than the silent swallow.
- A line-leading `#\'` comment is unchanged unless it strands a later quote; then
  the [L0001] it provokes names it.
- `needs_continuation` does not match `RawDelimiterEscaped`, so a REPL line
  `echo #\'…` reports the error at once rather than waiting for a continuation
  that will never arrive.

## Reading of "do not change the grammar" (load-bearing — needs confirmation)

The whole shape above rests on reading **"`#\'` stays a comment"** as *"do not
reclassify `#\'` as a raw-string opener"*: the `#'` rule is untouched, and the
lexer merely refuses the suspicious comment with a diagnostic. Under that reading
the value-position case is a hard error. The alternative reading — *the byte
sequence must still tokenise as a comment, only the surrounding diagnostics
improve* — forbids the hard error and collapses the fix to the line-leading
back-pointer alone. **This is the one decision the design cannot make for itself.**

## Open questions

- **Dedicated variant vs. `Other`.** A free-form `Other(msg)` would carry the
  message without a stable code or a tailored label. The dedicated
  `RawDelimiterEscaped` matches the module's structured-arm design and gives the
  doc an [L0004] to cite. *Proposed: dedicated.*
- **`\"` lumped with `\'`.** `#\"` is diagnosed by the same arm and named against
  `#'`; "remove the backslash" leaves `#"`, still not a delimiter, so the advice
  is imperfect for the double-quote case. *Proposed: keep them together — the
  message still names the right delimiter.*
- **Clearing on close.** Keeping the near-miss memory only until the next
  successful string close is what stops false positives across unrelated quotes.
  *Proposed: keep.*
- **One secondary slot vs. many.** Pointing at the near-miss costs the EOF
  pointer, because `render_ariadne` carries a single secondary label. Widening it
  to a list is a larger change to a shared path. *Proposed: repurpose the one
  slot.*

See also [[map/core/syntax|syntax]], [[map/core/diagnostics|diagnostics]],
[[internals/surface-syntax|surface-syntax]]; the lexer rules at
`core/src/syntax/lexer.rs` (the module docstring, the `#` dispatch in
`next_token`, and `scan_separator`).
