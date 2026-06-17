---
generated_at_commit: deac2a81
generated_at_date: 2026-06-11
covers_paths: [ral/src/repl/frontend.rs, ral/src/repl/frontend/, ral/src/repl/complete.rs]
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

Two implementations live in `frontend/`:

- `minimal.rs::MinimalFrontend` — canonical-stdin fallback for dumb terminals
  and `RAL_INTERACTIVE_MODE=minimal`. No raw mode, no DECSET, no plugin
  features; just `read_line` with a `> ` continuation prompt.
- `rustyline.rs::RustylineFrontend` — the real editor: completion, plugin
  keybindings, ghost text, highlights, and rustyline history, on TTYs that
  support raw mode and ANSI.

## Completion

`complete.rs::RalHelper` implements rustyline's `Completer`, `Hinter`, and
`Highlighter`. Completion classifies the token under the cursor as
variable / command / path and offers candidates accordingly. Ghost text and
syntax highlights are *not* computed here — they come from plugin
`buffer-change` hooks recorded in the [[map/repl/plugins|`PluginRuntime`]], and
`RalHelper` only paints what the runtime last produced.
