---
generated_at_commit: 738fa73
generated_at_date: 2026-06-20
covers_paths: [ral/src/repl/frontend.rs, ral/src/repl/frontend/, ral/src/repl/completion.rs, ral/src/repl/complete.rs]
---

# Map: repl / frontend

`ral/src/repl/frontend.rs` declares the `Frontend` trait — the seam between the
[[map/repl/loop|session loop]] and the line editor. One method, `read`, returns
a typed `Read` event (`Line` / `Edit` / `Interrupt` / `Eof`); the session never
sees keybindings, escape sequences, or the buffer stack. The frontend owns
plugin sync, keybinding dispatch, continuation reads, and flushing deferred
diagnostics before returning. `EditBuffer` carries a re-feed buffer whose
`cursor` is a **character** offset (matching the plugin surface — see
[[map/repl/plugins|plugins]]); the rustyline boundary converts to bytes.

Both backends drive the multi-line read through `frontend::join_continuation`,
which folds backend-supplied lines into the buffer until it parses complete
(or is abandoned by a `Continuation::Discard`). The loop, the newline joining,
and the abandon semantics live there once, so Ctrl-C / Ctrl-D / a read error
behave identically — abandoning the partial buffer keeps the session alive
rather than ending it (the prior per-frontend copies had drifted, letting
Ctrl-D kill the shell from the rustyline path). History saves *append* the
session's own entries rather than rewriting the file, so concurrent sessions do
not clobber each other.

Three implementations live in `frontend/`, selected by `Surface`
(`--surface` flag or rc `surface:`; the capability gate forces `minimal` on
dumb terminals whatever was asked):

- `minimal.rs::MinimalFrontend` — canonical-stdin fallback for dumb terminals
  and `RAL_INTERACTIVE_MODE=minimal`. No raw mode, no DECSET, no plugin
  features; just `read_line` with a `> ` continuation prompt.
- `rustyline.rs::RustylineFrontend` — the default editor: completion, plugin
  keybindings, ghost text, highlights, and rustyline history, on TTYs that
  support raw mode and ANSI.
- `structural.rs::StructuralFrontend` — the ratatui inline-viewport projection
  surface (`structural` feature, `--surface structural`): the typed spine,
  worksheet, and handles matrix around the prompt, plus Tab completion (below).
  See [[decisions/260620_repl-as-structural-surface|repl-as-structural-surface]].

## Completion

The completion *engine* is frontend-neutral: `completion.rs` classifies the
token under the cursor (`$`-variable / command-position name / path), gathers
candidates from a `Sources` snapshot of the live shell (PATH commands +
builtins + handlers + bindings; cwd-anchored path entries), and ranks them.
`completion::complete(line, pos, &Sources) -> (replace_from, Vec<Candidate>)`
is the single entry point both surfaces call. Ranking is `nucleo` fuzzy
matching for every surface — path-tuned for path entries, ties broken
alphabetically.

- `complete.rs::RalHelper` is the **rustyline adapter**: it holds the `Sources`
  snapshot (rebuilt each prompt via `refresh`), delegates `Completer::complete`
  to the engine, and maps each `Candidate` to a rustyline `Pair`. It also
  implements `Hinter`/`Highlighter` — ghost text and syntax highlights are *not*
  computed here; they come from plugin `buffer-change` hooks recorded in the
  [[map/repl/plugins|`PluginRuntime`]], and `RalHelper` only paints what the
  runtime last produced. `style_ansi` is the source of truth for the legal
  highlight-style vocabulary `_ed-highlight` validates against.
- `structural.rs` drives the engine as a **drop-down menu band**: Tab completes
  the token under the cursor — a unique match is spliced in place
  (`apply_candidate`), several open a bordered popup (`render_menu`) over the
  top of the projection band, anchored under the token. Tab/↓ and ⇧Tab/↑ cycle
  the selection, Enter accepts, Esc (or any editing key) dismisses. The lower
  band reserves room for the menu so a fresh session still has space to drop it
  down.
