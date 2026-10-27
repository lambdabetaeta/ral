---
generated_at_commit: 67715a25
generated_at_date: 2026-10-26
covers_paths: [ral/src/repl/plugin.rs, ral/src/repl/plugin/, ral/src/repl/keybinding.rs, ral/src/repl/enquiry.rs]
---

# Map: repl / plugins

The plugin runtime — the hook model, the `_ed-*` editor builtins,
keybindings, and the load builtins plugins program against. **A plugin is split
at the protocol: its hooks and aliases live engine-side, in the shell's hook
table, and never cross; its editor state, keybinding table, and first-order
manifest live host-side, in the REPL's `PluginRuntime`; the two meet only
through dispatches one way and two enquiry classes the other.** Editor state
is a host concern, so core holds none of it
([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]).
The runtime is frontend-neutral: it drives the in-editor plugin surface —
ghost text, highlight overlays, chord dispatch — that the rustyline editor renders ([[map/repl/frontend|frontend]]); this page owns
the runtime, that page owns the rendering.

## The hook model

A *hook* is a handler a plugin's manifest names for one of six events (the
`HookEvent` enum, `manifest.rs`, whose kebab-case names are the manifest keys),
registered into the shell's hook table under `HookName::plugin(name, event)`
by the load builtin, with the policy its event needs
(`load.rs::register_plugin_hooks`, an exhaustive match on the event): all as `HookSig::Hook { kind }`, all
`Denied` terminal authority, `buffer-change` and `complete` taken `aside`,
`prompt` taken with capture like the session prompt; a keybinding's
handler as `key#<index>` (its position in the manifest, so two guarded bindings on one key stay distinct) with `DefaultPolicy::leased()`, so an `_ed-tui` body
can foreground a captured pipeline. **A keybinding handler is the only run
that holds the terminal**, which is all `_ed-tui` asks.

**Running a hook is a dispatch.** `ReplHost::run_hook` sends one
`Program::Hook` run — with an optional wall, and optionally a `PluginContext`
installed host-side for that dispatch — and answers a `HookResult`. The
engine applies the registered policy: `aside` runs the hook on a
`Shell::join_session` sibling, which *shares* the session's cancel root, so a
Ctrl-C struck while a hook runs reaches it, while one aimed at a command
already in flight is older than every frame the aside mints
([[decisions/260726_cancel-is-a-watermark|cancel-is-a-watermark]]), and
nothing it does flows back. Plugins run with the host authority the hook run
carries (`GrantStack::root()`).

- **Lifecycle** — `exec.rs::fire(t, host, event, record)` dispatches the event
  on every plugin registering it, with the one event record `{src, status}` or
  `{old, new}` ([[map/repl/loop|`exec.rs`]] fires them after each line). A
  fault prints at once; the value is ignored.
- **Pre-exec** — [[map/repl/loop|`exec.rs::pre_exec`]] folds the plugins'
  `pre-exec` hooks over the line, each a dispatch with `{src}` whose `Str`
  result is the next hook's line. A `()` or an `exit` leaves the line; so do a
  fault and a value of another shape, each reported (the latter as a plugin
  warning), so no hook can stop every line.
- **Prompt** — [[map/repl/loop|`prompt.rs`]] folds every plugin `prompt` hook
  over the session prompt's text, reading each result as the session prompt
  is read.
- **Buffer-change** — `run_buffer_change_hooks`, called from rustyline's
  `Hinter`, dispatches each plugin's hook
  whenever the line or cursor moves, with `{old}` (the record carries only what the event alone knows; the
  editor is read through `_ed-*`), then gathers the
  ghost text and highlight spans the handlers left in the context.
- **Complete** — `run_complete_hooks`, called from `RalHelper`'s `Completer`
  ([[map/repl/frontend|frontend]]), reads the simple command at the cursor with
  `completion::parse_at` and gives each plugin's `complete` hook the
  `CommandAt` record `{words, current, offset}` — the one `_ed-parse`
  answers. A handler's value decodes as `Vec<Candidate>` (`Candidate`'s
  `Datum`: a `Str` is both fields, else exactly `{replacement, display}`);
  the first non-empty list claims the word from `offset`, an empty one or `()`
  passes on, and a mis-shaped value defers a plugin warning and passes. `None` — no
  handlers, a buffer that does not lex, no claim — leaves the engine's
  `completion::complete` to answer. Whatever the hook wrote to the editor is
  dropped; only its state cell is kept.
- **Keybinding** — `keybinding.rs::dispatch_keybinding` (below).

`buffer-change` and `complete` both go through `plugin.rs::run_braked`, under
the event's `Brake` (its event and wall, `BUFFER_CHANGE` or `COMPLETE`): it
reads the plugin's state cell, the history and the configured keymap under one
lock, dispatches with a wall and a fresh `PluginContext`, then under one lock
saves the state cell back, defers any fault, and folds the outcome into the
breaker.

The runtime's lock is never held across a dispatch: the host answers the
dispatch's `repl-editor` enquiries from that very state.

## Faults and the breaker

A hook fault is rendered by the engine against the session's source registry,
where the load registered the plugin's text, so it points at the plugin
file's line exactly as a command fault does, and it is labelled
`plugin 'p' hook 'h'` by `HookName::fault_label` — every plugin fault prints
one shape. A hook inside the readline loop defers its fault through
`defer_plugin_message` past the line-erase escape, flushed by
`flush_pending_messages` ([[map/repl/frontend|frontend]]); a lifecycle hook
prints at once.

**A buffer-change hook fires on every keystroke and a complete hook on every
Tab, so a slow or faulting one is braked.** `HookHealth` (per plugin and event,
in `LoadedPlugin::health`) is a circuit breaker: `FAULT_LIMIT` consecutive
faults, or any run overrunning the wall its dispatch arms
(100 ms for `buffer-change`, 500 ms for `complete`), trips it and the hook is
skipped for the rest of the session; any other ending resets the count.
`HookHealth::record_outcome` folds the dispatch's `Outcome` (`host.rs`) in.
`PluginRuntime::with_hook(event)` names the plugins registering `event` whose
breaker has not tripped, which is every caller's one list of handlers. The trip
emits one disable notice (`errfmt::format_plugin_disabled`). The wall is
cooperative — the machine polls cancellation, so an overrunning handler is
preempted.

## The `_ed-*` builtins

`plugin/ed_builtins.rs` defines `ED_BUILTINS`, the line-editor builtin family,
one `BuiltinEntry` per op so the typechecker sees each return type and arity is
fixed per op ([[invariants/fixed-arity|fixed-arity]]). The `_` prefix hides
them from `help`. They split into reads (`_ed-get`, answering the whole `EditorSnapshot`), buffer writes (`_ed-set`, `_ed-set-lbuffer`,
`_ed-insert`, `_ed-push`, `_ed-accept`), display effects (`_ed-ghost`,
`_ed-highlight`), services (`_ed-tui`, `_ed-history`, `_ed-parse`, `_ed-state`),
and terminal escapes (`_ed-clipboard` copies natively when local, else by OSC 52; `_ed-hyperlink` is OSC 8).

Each is a **thin engine-side builtin**: it keeps its own checks — the grant, via
`ask`/`answer` (`Shell::check(op.flag(), name)` then the enquiry), and its
arguments' shape — and puts the rest to the host as one enquiry, decoding the
answer strictly. **A grant follows the request**: `enquiry::Gated` names the
grant a request needs (`flag`) and turns into an `Enquiry`, implemented by
`EditorOp` (`` `repl-editor ``) and `ClipboardReq` (`` `repl-clipboard ``,
`editor.write`), so a builtin checks every grant it needs before its first effect
— `_ed-state`, whose cell read needs `read` and write `write`, checks `write`
first, before its updater runs. `_ed-tui` needs `editor.tui` and the terminal, not the
editor: `Mooring::lend_terminal` is `Some` only from `Leased` (a keybinding
handler's run), so anywhere else, or nested, it answers status 1 with a
sentence saying this run holds no terminal to lend. Its captured stdout is
decoded lossily after removing one LF or CRLF (`repl::captured_line`, over `io::without_terminator`);
a lone CR, embedded NULs, and preceding terminators remain. Captured prompt
text follows the same rule; returned prompt Strings do not. The host answers an editor
op only while a `PluginContext` is installed for the dispatch — inside a
plugin handler — and refuses otherwise with "no plugin context". The builtins ride the
REPL surface the `repl` recipe boots, so the typechecker reads them off the
session's own builtin table from the first rc check.

## Context and editor state

`plugin/editor.rs` holds the host-side types. `PluginContext` is installed by
`ReplHost` around one hook's dispatch and taken back after it: the live
`editor_state` (private fields: `apply` clamps the cursor into the text after
every op) with its `Keymap`, the `history` (an `Arc<[String]>`), the `outputs` (ghost text,
highlight spans, pushed buffer, accept flag), and the plugin's `state_cell`,
saved back onto its `LoadedPlugin` afterwards. `PluginContext::apply` answers
one `EditorOp`; `Get` answers `EditorSnapshot {text, cursor, keymap}`, whose
`keymap` is the configured one (`emacs` or `vi`, the `Keymap` datum), never the
live vi mode — rustyline has no mode-change callback, so that rides a
keybinding's record as `mode` (below). All cursor offsets here are **character** offsets; core's
`text::char_to_byte` / `text::byte_to_char` convert at the rustyline boundary
so plugin code never handles UTF-8.

## The enquiry vocabulary

`enquiry.rs` types the REPL's three classes once, for builtin and host alike:
`` `repl-editor `` carries an `EditorOp` (`get`, `set`, `set-lbuffer`,
`insert`, `push`, `accept`, `ghost`, `highlight`, `history`, `state-get`,
`state-set`) and answers `EditorSnapshot` or data; `` `repl-plugin `` carries
a `PluginNote` — `loaded` with the manifest's first-order part, `unloaded`
with a name; `` `repl-clipboard `` carries a `ClipboardReq` and answers
whether the whole text landed. `EditorOp` and `ClipboardReq` are `Gated`. An
unknown class or tag is refused by name.

## Runtime, manifests, loading

`plugin.rs::PluginRuntime` is the `Arc<Mutex<…>>` the `ReplHost` holds,
threaded between the loop, rustyline's `Hinter`/`Highlighter` callbacks, and keybinding dispatch. It holds the
host's plugin list (`LoadedPlugin`s, told by the load builtins), the `KeyRouter`,
and the `keybindings_dirty` flag directly and partitions the rest into
`EditorHooks`, `Keybindings`, and `DeferredDiagnostics` so each call site
reaches only its slice. `PluginRuntime::note` takes what a load builtin says:
`admit` re-validates a manifest into a `LoadedPlugin` (chords parsed, guard
regexes compiled) — the engine's own admission checks do not excuse it — and
may refuse, which the builtin acts on.

The frontend-neutral key vocabulary lives in `plugin/router.rs`:
`parse_key_notation` yields a sequence of `KeyChord`s (a `KeyName` plus
`ctrl`/`alt`/`shift`) that rustyline adapts to `KeyEvent`s
(`chord_to_key_event`; rustyline normalises shift-tab itself); the grammar is
the SPEC's (§15.6), and a refusal is one sentence naming the offending text
(a modifier twice, a non-key, a control character, and the spellings a
terminal cannot tell apart — `shift` with a character, `ctrl` with an uppercase
letter, `ctrl-h`/`i`/`m`/`[` — each with "did you mean …?", other modifiers
kept). `Keymap` (`Emacs` / `Vi`) reduces rustyline's
`EditMode`. **Keybinding dispatch is one ordered router**: `KeyRouter` — held
on the runtime, rebuilt by `keybindings_changed` whenever the plugin list
changes — flattens every binding in load order (then manifest order within a
plugin), and `resolve` returns the first entry whose chord *sequence* equals
the one pressed and whose `guard` regex (matched against the text left of the
cursor) allows.
`Resolution::Claimed` names the owning plugin and binding index;
`Resolution::Default` is the editor's built-in tail, which each backend
realises natively (rustyline's per-sequence `RouterKeyHandler` returns `None`,) — precedence is decided
once, so the frontends cannot disagree. Resolution is pure host-side work,
safe inside editor callbacks where no dispatch may run.

**Sequences are rustyline's.** rustyline binds `Event::KeySeq` in its own trie
but consults sequences only after `ctrl-x` (emacs) or a key the keymap leaves
unbound, so `KeySpec::bind` requires a sequence to begin with plain `ctrl-x`.
A bound *prefix* is consulted before rustyline reads on, so an unguarded
binding on a proper prefix of a sequence kills it: `dead_entries` pairs each
entry with its blocker — an unguarded entry on the same sequence loaded
earlier, or on a proper prefix wherever loaded.

- `plugin/manifest.rs` — a manifest is the Map a plugin's top-level block
  returns. `parse` splits it into the first-order `Manifest` (`{name, hooks,
  keybindings: [{key, guard}]}`) the host is told and the `ManifestHandlers`
  (hook, keybinding, and alias values) the engine keeps. A `KeySpec` is
  validated at load — notation parsed, optional `guard:` regex compiled:
  `ctrl-c`/`ctrl-d` are reserved outright, as any chord of a sequence
  (`reserved_action`, the session's escape hatches); a sequence not beginning
  with `ctrl-x` is refused; and an unguarded binding on a single chord
  carrying a ral-owned built-in action (`builtin_action` — every unmodified
  key except F1–F12, none once any modifier is set) is a load error, so
  same-key bindings compose as an ordered match with the built-in as the
  final arm. A `capabilities:` key is a load error, not
  silent confinement — plugins run with host authority; to attenuate, wrap
  the invocation in `grant { … }`. It is not an *unknown* key either: the
  manifest's declared table (`typecheck::contract`, `Form::Manifest`) marks
  it refused, with that advice as its own sentence. The table also closes the
  keyset — `name` (required, `String`), `aliases`, `hooks`, `keybindings` —
  so an unknown key is an error naming the list, here and in the checker
  both; `hooks:`/`keybindings:`/`aliases:` stay on this file's own runtime
  parse for their *shape*, their handler values having no one fixed type to
  pin.
- `plugin/load.rs` — the `load-plugin` and `unload-plugin` builtins (`BUILTINS`),
  engine-side static builtins on the REPL surface. `load_plugin` resolves a
  plugin under `~/.config/ral/plugins/` or `RAL_PATH`, typechecks and
  evaluates it through `shell::load::evaluate_source` in a fresh scope, under
  the return contract `contract::declared(Form::Manifest)`; `instantiate`
  applies a parameterised plugin's factory to its options map, nested under
  the builtin's own mooring, to yield its manifest (the contract sees only a
  literal `return [...]`, so a factory's manifest stays on `manifest::parse`'s
  runtime check alone). Options arrive as a `Map`: the rc's own `plugins:`
  entry is where a non-map value is caught and named, and `load-plugin`,
  which takes a name alone, passes the empty map. The builtin then commits the
  hooks and aliases and enquires `` `repl-plugin `loaded ``; any failure
  past the commit — the host's refusal included — rolls the plugin's whole
  namespace back. The builtins return their outcome: a failed `load-plugin` or `unload-plugin`
  raises, its error contexted `plugin 'NAME'`, and an `exit` escape
  propagates. A relative path resolves through `shell.resolve` (live cwd,
  `~`). The plugin ledger `PluginEntry { name, aliases }` lives in the session's
  `HookTable` beside the hooks it undoes, aliases by `FrameHandle` so an
  alias the user replaced survives unload. Unloading is the exact inverse:
  the host is told first and may refuse, then the hooks and aliases go. Only
  the booted session has a `HookTable`; a load from an aside, a worker or a
  stage fails at once with `RegisterError::NoSession`. Loading also runs the
  shadow lint host-side (`PluginRuntime::admit`): a binding the router's
  `dead_entries` flags is warned about, not rejected, when either it or its
  blocker belongs to the plugin being loaded — a later unguarded prefix can
  kill an earlier plugin's sequence.
- `keybinding.rs` — when a plugin-registered key fires, rustyline stashes a
  `PendingKeybinding` and accepts the line; `dispatch_keybinding` then
  dispatches the handler's hook outside the readline borrow with a fresh
  context, resolving the owning plugin by **name**
  (`PluginRuntime::resolve_keybinding`, which also yields the key notation the
  handler's `[key, mode]` record carries) and saving its `state_cell` back
  exactly as the braked path does. `mode` is `insert`, `command` or
  `replace`: the editor's input mode at the press, which rustyline's
  `EventContext::input_mode()` gives the handler (`RouterKeyHandler`) and
  `PendingKeybinding.mode` carries across the accepted line (`insert` in
  emacs). A `PendingKeybinding` carries the
  plugin's name plus a binding index *within* it, never a position in the
  runtime `Vec` — an unload compacts that vector, so a runtime index would
  address the wrong plugin; a resolution miss re-edits the line unchanged.
- `plugin/rustyline.rs` is the runtime's rustyline boundary: `sync_plugins`
  reconciles rustyline's binding table by full unbind-then-rebind,
  registering one `RouterKeyHandler` per distinct bound sequence
  (`bound_keys`, in `plugin/router.rs`), as an `Event::KeySeq` of
  `chord_to_key_event`s, that consults the live router on each press. The
  bound events live on `RustylineFrontend` (`Vec<Event>`), passed to
  `sync_plugins` by `&mut`. `CtrlDHandler` and the rustyline `chord_to_key_event` conversion
  live here too, over the `RalHelper` editor rustyline adapts against
  ([[map/repl/frontend|frontend]]).
