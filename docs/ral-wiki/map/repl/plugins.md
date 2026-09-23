---
generated_at_commit: e2b7067b
generated_at_date: 2026-09-23
covers_paths: [ral/src/repl/plugin.rs, ral/src/repl/plugin/, ral/src/repl/keybinding.rs, ral/src/repl/enquiry.rs]
---

# Map: repl / plugins

The plugin runtime — the hook model, the `_ed-*` editor builtins,
keybindings, and the load doors plugins program against. **A plugin is split
at the protocol: its hooks and aliases live engine-side, in the shell's hook
table, and never cross; its editor state, keybinding table, and first-order
manifest live host-side, in the REPL's `PluginRuntime`; the two meet only
through dispatches one way and two enquiry classes the other.** Editor state
is a host concern, so core holds none of it
([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]).
The runtime is frontend-neutral: it drives the in-editor plugin surface —
ghost text, highlight overlays, chord dispatch — that both the rustyline and
structural editors render ([[map/repl/frontend|frontend]]); this page owns
the runtime, that page owns the rendering.

## The hook model

A *hook* is a handler a plugin's manifest names for one of five events,
registered into the shell's hook table under `HookName::plugin(name, event)`
by the load door, with the policy its event needs
(`load.rs::register_plugin_hooks`): `pre-exec`, `post-exec` and `chpwd` as
`HookSig::Lifecycle`, `prompt` and `buffer-change` as `HookSig::Hook`, all
`Denied` terminal authority, `buffer-change` taken `aside`; a keybinding's
handler as `key:<chord>` with `DefaultPolicy::leased()`, so an `_ed-tui` body
can foreground a captured pipeline.

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

- **Lifecycle** — `plugin::fire(t, host, event, record)` dispatches the event
  on every plugin registering it, with the one event record `{src}`,
  `{src, status}` or `{old, new}` ([[map/repl/loop|`exec.rs`]] fires them
  around each line). A fault prints at once.
- **Prompt** — [[map/repl/loop|`prompt.rs`]] folds every plugin `prompt` hook
  over the session prompt's text.
- **Buffer-change** — `run_buffer_change_hooks`, called from rustyline's
  `Hinter` (and the structural surface's tick), dispatches each plugin's hook
  whenever the line or cursor moves, with `{old_buf, line, pos, history,
  keymap, state}` and a context with `in_readline` set, then gathers the
  ghost text and highlight spans the handlers left in it.
- **Keybinding** — `keybinding.rs::dispatch_keybinding` (below).

The runtime's lock is never held across a dispatch: the host answers the
dispatch's `repl-editor` enquiries from that very state.

## Faults and the buffer-change breaker

A hook fault is rendered by the engine against the session's source registry,
where the load registered the plugin's text, so it points at the plugin
file's line exactly as a command fault does, and it is labelled
`plugin 'p' hook 'h'` by `HookName::fault_label` — every plugin fault prints
one shape. A hook inside the readline loop defers its fault through
`defer_plugin_message` past the line-erase escape, flushed by
`flush_pending_messages` ([[map/repl/frontend|frontend]]); a lifecycle hook
prints at once.

**A buffer-change hook fires on every keystroke, so a slow or faulting one is
braked.** `HookHealth` (per plugin, on its `LoadedPlugin`) is a circuit
breaker: `BUFFER_CHANGE_FAULT_LIMIT` consecutive faults, or any run overrunning
the `BUFFER_CHANGE_BUDGET` wall its dispatch arms, trips it and the hook is
skipped for the rest of the session; a success resets the count. The trip
emits one disable notice (`errfmt::format_plugin_disabled`). The wall is
cooperative — the machine polls cancellation, so an overrunning handler is
preempted.

## The `_ed-*` builtins

`plugin/ed_builtins.rs` defines `ED_BUILTINS`, the line-editor builtin family,
one `BuiltinEntry` per op so the typechecker sees each return type and arity is
fixed per op ([[invariants/fixed-arity|fixed-arity]]). The `_` prefix hides
them from `help`. They split into reads (`_ed-get`, `_ed-text`, `_ed-cursor`,
`_ed-keymap`, `_ed-lbuffer`), buffer writes (`_ed-set`, `_ed-set-lbuffer`,
`_ed-insert`, `_ed-push`, `_ed-accept`), display effects (`_ed-ghost`,
`_ed-highlight`), services (`_ed-tui`, `_ed-history`, `_ed-parse`, `_ed-state`),
and terminal escapes (`_ed-clipboard` for OSC 52, `_ed-hyperlink` for OSC 8).

Each is a **thin engine-side door**: it keeps its own checks — interactive
mode, `check_editor_read`/`check_editor_write` against the grant, its
arguments' shape — and puts the rest to the host as one `` `repl-editor ``
enquiry (`EditorOp`), decoding the answer strictly. The host answers only
while a `PluginContext` is installed for the dispatch — inside a plugin
handler — and refuses otherwise with "no plugin context". The doors ride the
REPL surface the `repl` recipe boots, so the typechecker reads them off the
session's own builtin table from the first rc check.

## Context and editor state

`plugin/editor.rs` holds the host-side types. `PluginContext` is installed by
`ReplHost` around one hook's dispatch and taken back after it: the live
`editor_state`, the `history`, `in_readline`, the `outputs` (ghost text,
highlight spans, pushed buffer, accept flag), and the plugin's `state_cell`,
saved back onto its `LoadedPlugin` afterwards. `PluginContext::apply` answers
one `EditorOp`. All cursor offsets here are **character** offsets; core's
`text::char_to_byte` / `text::byte_to_char` convert at the rustyline boundary
so plugin code never handles UTF-8.

## The enquiry vocabulary

`enquiry.rs` types the REPL's two classes once, for door and host alike:
`` `repl-editor `` carries an `EditorOp` (`get`, `set`, `set-lbuffer`,
`insert`, `push`, `accept`, `ghost`, `highlight`, `history`, `state-get`,
`state-set`) and answers `EditorSnapshot` or data; `` `repl-plugin `` carries
a `PluginNote` — `loaded` with the manifest's first-order part, `unloaded`
with a name. An unknown class or tag is refused by name.

## Runtime, manifests, loading

`plugin.rs::PluginRuntime` is the `Arc<Mutex<…>>` the `ReplHost` holds,
threaded between the loop, rustyline's `Hinter`/`Highlighter` callbacks, the
structural surface's per-tick loop, and keybinding dispatch. It holds the
host's plugin list (`LoadedPlugin`s, told by the load doors), the `KeyRouter`,
and the `keybindings_dirty` flag directly and partitions the rest into
`EditorHooks`, `Keybindings`, and `DeferredDiagnostics` so each call site
reaches only its slice. `PluginRuntime::note` takes what a load door says:
`admit` re-validates a manifest into a `LoadedPlugin` (chords parsed, guard
regexes compiled) — the engine's own admission checks do not excuse it — and
may refuse, which the door acts on.

The frontend-neutral key vocabulary lives in `plugin/router.rs`:
`parse_key_notation` yields a `KeyChord`/`KeyName` that rustyline adapts to its
`KeyEvent` (`chord_to_key_event`) while the structural surface matches
crossterm's against it; `Keymap` (`Emacs` / `Vi`) reduces rustyline's
`EditMode`. **Keybinding dispatch is one ordered router**: `KeyRouter` — held
on the runtime, rebuilt by `keybindings_changed` whenever the plugin list
changes — flattens every binding in load order (then manifest order within a
plugin), and `resolve` returns the first entry whose chord matches and whose
`guard` regex (matched against the text left of the cursor) allows.
`Resolution::Claimed` names the owning plugin and binding index;
`Resolution::Default` is the editor's built-in tail, which each backend
realises natively (rustyline's per-chord `RouterKeyHandler` returns `None`,
the structural surface falls into its own key arms) — precedence is decided
once, so the frontends cannot disagree. Resolution is pure host-side work,
safe inside editor callbacks where no dispatch may run.

- `plugin/manifest.rs` — a manifest is the Map a plugin's top-level block
  returns. `parse` splits it into the first-order `Manifest` (`{name, hooks,
  keybindings: [{key, guard}]}`) the host is told and the `ManifestHandlers`
  (hook, keybinding, and alias values) the engine keeps. A `KeySpec` is
  validated at load — chord parsed, optional `guard:` regex compiled:
  `ctrl-c`/`ctrl-d` are reserved outright (`reserved_action`, the session's
  escape hatches), and an unguarded binding on a chord carrying a ral-owned
  built-in action (`builtin_action` — every unmodified key except F1–F12) is
  a load error, so same-chord bindings compose as an ordered match with the
  built-in as the final arm. A `capabilities:` key is a load error, not
  silent confinement — plugins run with host authority; to attenuate, wrap
  the invocation in `grant { … }`. It is not an *unknown* key either: the
  manifest's declared table (`typecheck::contract`, `Form::Manifest`) marks
  it refused, with that advice as its own sentence. The table also closes the
  keyset — `name` (required, `String`), `aliases`, `hooks`, `keybindings` —
  so an unknown key is an error naming the list, here and in the checker
  both; `hooks:`/`keybindings:`/`aliases:` stay on this file's own runtime
  parse for their *shape*, their handler values having no one fixed type to
  pin.
- `plugin/load.rs` — the `load-plugin` and `unload-plugin` doors (`DOORS`),
  engine-side static builtins on the REPL surface. `load_plugin` resolves a
  plugin under `~/.config/ral/plugins/` or `RAL_PATH`, typechecks and
  evaluates it through `modules::evaluate_source` in a fresh scope, under
  the return contract `contract::declared(Form::Manifest)`; `instantiate`
  applies a parameterised plugin's factory to its options map, nested under
  the door's own mooring, to yield its manifest (the contract sees only a
  literal `return [...]`, so a factory's manifest stays on `manifest::parse`'s
  runtime check alone). Options arrive as a `Map`: the rc's own `plugins:`
  entry is where a non-map value is caught and named, and `load-plugin`,
  which takes a name alone, passes the empty map. The door then commits the
  hooks and aliases and enquires `` `repl-plugin `loaded ``; any failure
  past the commit — the host's refusal included — rolls the plugin's whole
  namespace back. The engine keeps its own registry of what it committed,
  `ReplScratch.plugins` (`{name, aliases}`), for `check_not_loaded` and
  unload. Unloading is the exact inverse: the host is told first and may
  refuse, then the hooks and aliases go. A load inside a `spawn`ed worker,
  which has no desk, fails with the no-desk error. Loading also runs the
  shadow lint host-side: a binding the router's `dead_entries` flags (an
  earlier unguarded entry owns its chord) is warned about, not rejected.
- `keybinding.rs` — when a plugin-registered key fires, rustyline stashes a
  `PendingKeybinding` and accepts the line; `dispatch_keybinding` then
  dispatches the handler's hook outside the readline borrow with a fresh
  context, resolving the owning plugin by **name**
  (`PluginRuntime::resolve_keybinding`) and saving its `state_cell` back
  exactly as the buffer-change path does. A `PendingKeybinding` carries the
  plugin's name plus a binding index *within* it, never a position in the
  runtime `Vec` — an unload compacts that vector, so a runtime index would
  address the wrong plugin; a resolution miss re-edits the line unchanged.
- `plugin/rustyline.rs` is the runtime's rustyline boundary: `sync_plugins`
  reconciles rustyline's binding table by full unbind-then-rebind,
  registering one `RouterKeyHandler` per distinct bound chord
  (`bound_chords`, in `plugin/router.rs`) that consults the live router on
  each press; a frontend matching keys itself snapshots the `KeyRouter`
  instead. `CtrlDHandler` and the rustyline `chord_to_key_event` conversion
  live here too, over the `RalHelper` editor rustyline adapts against
  ([[map/repl/frontend|frontend]]).
