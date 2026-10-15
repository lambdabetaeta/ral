---
generated_at_commit: 4e2ee042
generated_at_date: 2026-10-12
covers_paths: [ral/src/repl/frontend.rs, ral/src/repl/frontend/, ral/src/repl/completion.rs, ral/src/repl/complete.rs, ral/src/repl/highlight_style.rs, prompt-editor/src/completion.rs, core/src/text.rs]
---

# Map: repl / frontend

`ral/src/repl/frontend.rs` declares the `Frontend` trait — the seam between the
[[map/repl/loop|session loop]] and the line editor. One method, `read`, returns
a typed `Read` event (`Line` / `Edit` / `Interrupt` / `End(status)`); the session never
sees keybindings, escape sequences, or the buffer stack. The frontend owns
plugin sync, keybinding dispatch, and the erase of an abandoned edit; the
session flushes deferred diagnostics once after `read` returns, and once in
`Drop`. **A frontend reads the engine only through the
transport `read` is handed** — probes, and hook dispatches through the
`ReplHost` — never a `Shell`. `EditBuffer` carries a re-feed buffer whose
`cursor` is a **character** offset (matching the plugin surface — see
[[map/repl/plugins|plugins]]); the rustyline boundary converts to bytes.

**The editor owns the whole buffer.** `RalHelper` implements rustyline's
`Validator` (`needs_continuation`), so Enter on an incomplete buffer inserts a
newline in place; a plugin key's `AcceptLine` submits unconditionally, so a
pending key always sees the full buffer and a full-buffer byte cursor. After an
`Edit` outcome `cursor::erase_drawn` erases exactly the rows drawn (per-line
`ceil(width / cols)`, prompt included, by `unicode-width`). `End(0)` is EOF, a
read error prints then `End(1)`. The minimal frontend alone folds continuation
lines (`join_continuation`); Ctrl-C there arrives as a signal, so a line read
after the `interrupt_epoch` moved restarts the buffer. Both frontends hold a
rustyline `FileHistory` (one `#V2` format); saves *append* the session's own
entries so concurrent sessions do not clobber each other, and a failed save is
reported once, from `Drop for Session`.

Two implementations live in `frontend/`, selected by `Surface`
(`--surface` flag or rc `surface:`; the capability gate forces `minimal` on
dumb terminals whatever was asked):

- `minimal.rs::MinimalFrontend` — canonical-stdin fallback for dumb terminals
  and `RAL_INTERACTIVE_MODE=minimal`. No raw mode, no DECSET, no plugin
  features; just `read_line` with a `> ` continuation prompt.
- `rustyline.rs::RustylineFrontend` — the default editor: completion, plugin
  keybindings, ghost text, highlights, and rustyline history, on TTYs that
  support raw mode and ANSI. Its `printer` hands rustyline's
  `ExternalPrinter` to the `ReplHost`, so a surfaced `` `watch `` line lands
  above the live prompt. It remembers the `Event`s it bound (`bound`), so
  `sync_plugins` can unbind them before rebinding: each distinct plugin key
  sequence is one `Event::KeySeq`, a lone chord a sequence of one
  ([[map/repl/plugins|plugins]]).

**On Unix, ending the session wakes its input without taking ownership of
cleanup.** `input.rs::Input` is the unique reader; its shared `Shutdown`
handle is held by the ambient forwarder.

- Minimal input reads through `Reading` itself, checking shutdown before
  each read and after interruption. The interactive SIGINT handler omits
  `SA_RESTART`, so a blocked read returns `EINTR`. Inherited descriptor flags
  are untouched: `O_NONBLOCK` belongs to the shared open file description,
  including the parent's aliases. Already-nonblocking input waits in `poll`
  after `EAGAIN`. One-byte buffering leaves the next command's stdin untouched.
- Rustyline construction requires its external printer on Unix: that
  establishes a supported input/output TTY and its SIGINT-aware reader.
  Construction failures retain their diagnostic and fall back to minimal.
- Both frontends hold a `Reading` guard, exclusively registering the reading
  pthread and temporarily unblocking SIGINT. Shutdown sends it SIGINT every
  20 ms until the guard drops:
  the retry covers entry before rustyline installs its handler or between
  the minimal reader's shutdown check and its blocking syscall. The guard
  cannot cross threads; unregistration precedes restoring the signal mask.
- Rustyline restores raw mode and bracketed paste before returning. The
  session observes the durable root before interpreting the returned input,
  then follows its ordinary history, worker, and terminal cleanup.

## Completion

The completion *engine* is frontend-neutral: `completion.rs` classifies the
atom under the cursor by the lexer's own tokens (`lex_open` over the line up to
the cursor, which also closes an open string or block): the last lexeme
touching the cursor, at command position iff the lexeme before it is absent or
`separates`. A path candidate replaces the *whole* atom from its start, as one
`quote_word`-rendered atom (`dir/'a b'` would not parse; `'dir/a b'`
does), tilde-expanded first when it needs quoting. It then gathers
candidates from a `Sources` view the engine's readings answer — the cheap
half (`completion-names`, `builtin-names`) once per prompt, the `PATH`
enumeration lazily (only when a command name is wanted, never for `$var` or
paths) through `path-entries` in the engine's own filesystem,
keyed by the `env-var PATH` and `cwd` readings and aged by `SCAN_TTL`;
cwd-anchored path entries likewise — and matches them.
`completion::complete(line, pos, &Sources) -> (replace_from, Vec<Candidate>)`
is the engine's single entry point; the rustyline helper reaches it only when
no plugin `complete` hook claims the word (below). Matching runs once, via
`matching`, over a single borrowed pool (`Item`: a name, or a path entry with its
directory): names extending the typed text (smart case, alphabetical), else
`nucleo` fuzzy matches over the same pool. Rustyline's List mode splices the
candidates' common prefix, so a mixed list would never extend what was typed. Command position also offers the cwd's directories as
`dir/`. `ral_core::text::rank` and `rank_by` are that fuzzy tier and also serve
exarch's pickers; `rank_by` takes the haystack as a projection out of the item.
`Candidate` is the engine's own type and stays here.

`completion::parse_at` reads the simple command around the cursor as a
`CommandAt {words, current, offset}` — `words[current]` the word being typed,
from char `offset` — which `_ed-parse` answers and a plugin `complete` hook
receives. `Candidate` is also a `Datum`: a bare string is both its fields, else
exactly a `{replacement, display}` record.

- `complete.rs::RalHelper` is the **rustyline adapter**: it holds the `Sources`
  snapshot (rebuilt each prompt via `refresh`), asks
  `plugin::run_complete_hooks` first — the first plugin `complete` hook to
  return a non-empty list claims the word, replacing from its `offset`
  ([[map/repl/plugins|plugins]]) — else asks the engine in
  `Completer::complete`, and maps each `Candidate` to a rustyline `Pair`. It also
  implements `Hinter`/`Highlighter` — ghost text and syntax highlights are *not*
  computed here; they come from plugin `buffer-change` hooks recorded in the
  [[map/repl/plugins|`PluginRuntime`]], and `RalHelper` only paints what the
  runtime last produced. `highlight_style.rs::STYLES` is the one data-driven
  table of legal highlight styles — each row gives the name, its ANSI escape
  (`style_ansi`, which `_ed-highlight` validates against).
