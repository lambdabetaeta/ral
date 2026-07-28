---
generated_at_commit: 19d53bb
generated_at_date: 2026-07-28
covers_paths: [ral/src/repl/frontend.rs, ral/src/repl/frontend/, ral/src/repl/completion.rs, ral/src/repl/complete.rs, ral/src/repl/highlight_style.rs]
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
  support raw mode and ANSI. Its constructor wires the shell's stdout onto
  rustyline's `ExternalPrinter`, so background `watch` output lands above the
  live prompt.
- `structural.rs::StructuralFrontend` — the ratatui inline-viewport projection
  surface (`structural` feature, `--surface structural`): the typed spine,
  worksheet, and handles matrix around the prompt, plus Tab completion (below).
  See [[decisions/260620_repl-as-structural-surface|repl-as-structural-surface]].
  It drives the same in-editor plugin surface the rustyline backend does, off the
  shared [[map/repl/plugins|`PluginRuntime`]] rather than a parallel copy: each
  iteration runs `run_buffer_change_hooks` and reads back the fish-style ghost
  suggestion and highlight spans (overlaid as ratatui cells via
  `highlight_style::style_ratatui`), accepts the ghost on right-arrow at buffer end,
  and resolves each keypress through a snapshot of the shared
  [[map/repl/plugins|`KeyRouter`]] (`Resolution::Default` falls into the
  surface's own built-in key arms). A claimed binding breaks the loop to a
  `Composed::Keybinding` outcome — no new `Read` variant — tears
  down the viewport and raw mode, then runs `dispatch_keybinding`, so an
  `_ed-tui` handler (fzf, zoxide) gets the terminal exactly as the rustyline path
  dispatches only after leaving `readline`; an `_ed-push` buffer is popped when
  nothing is pending.

The structural surface and exarch TUI share their prompt editor through
the `prompt-editor` crate (backed by `edtui` 0.11): the `PromptEditor` facade
owns all cursor positioning, key dispatch (Emacs and Vim via edtui
`EditorEventHandler`), height hinting, and highlight rendering. Both
frontends show the terminal native cursor in **every** mode, the same shape
throughout (no painted vi modal-mode block). One implementation, not a copy each.
The structural surface handles shell-line chords before the editor:
**Ctrl-U kills to line-start** (readline unix-line-discard, not edtui
undo), in emacs and in vi-Insert mode. vi Normal/Visual keep the edtui keymap.
Ctrl-D on a non-empty buffer deletes the char under the cursor;
only an empty buffer reads as `Eof`.

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
  runtime last produced. `highlight_style.rs::STYLES` is the one data-driven
  table of legal highlight styles — each row gives the name, its ANSI escape
  (`style_ansi`, which `_ed-highlight` validates against), and its ratatui cell
  style (`style_ratatui`) — so the two surfaces cannot drift.
- `structural.rs` drives the engine as a **drop-down menu band**
  (`structural/menu.rs`): Tab completes the token under the cursor — a unique
  match is spliced in place, several open a bordered popup rendered over the
  top of the projection band, anchored under the token. Tab/↓ and ⇧Tab/↑ cycle
  the selection, Enter accepts, Esc (or any editing key) dismisses. The lower
  band reserves room for the menu so a fresh session still has space to drop it
  down.
