---
generated_at_commit: fc49779
generated_at_date: 2026-07-23
covers_paths: [ral/src/repl/plugin.rs, ral/src/repl/plugin/, ral/src/repl/plugin_editor.rs, ral/src/repl/plugin_ed_builtins.rs, ral/src/repl/keybinding.rs, ral/src/repl/host_handlers.rs]
---

# Map: repl / plugins

The plugin runtime — the hook model, the `_ed-*` editor builtins, keybindings,
and captured session commands that plugins program against. It lives entirely
in the `ral` crate because editor state is a host concern; core stores the
context type-erased as `Box<dyn Any>` in `ReplScratch.plugin_context` and never
inspects it ([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]).
**This runtime is frontend-neutral: it drives the in-editor plugin surface — ghost
text, highlight overlays, chord dispatch — that both the rustyline and structural
editors render** ([[map/repl/frontend|frontend]]); this page owns the runtime, that
page owns the rendering.

## The hook model

A *hook* is a thunk a plugin registers for one of five events — registered
into the shell's hook table under a `HookName` at load time.
`call_plugin_hook` in `plugin.rs` is the single primitive that runs one. It
brackets the call by taking the live `plugin_context` aside, installing the
per-call one so `_ed-*` builtins resolve, applying the handler, then taking
the (now-populated) context back and restoring the prior one. *Framing*
decides whether the handler runs in the caller's turn or a fresh one:

- `HookFraming::InFrame` — lifecycle hooks (`pre-exec`, `post-exec`, `chpwd`)
  fire from inside the command's own turn frame, so the handler is applied in
  place; a second frame would nest. `run_lifecycle_hook` (a `fold_hook` over
  the loaded plugins) is the entry [[map/repl/loop|`exec.rs`]] calls around
  each dispatch.
- `HookFraming::Framed(FramedHook)` — keybinding, buffer-change, and prompt
  hooks fire during the frontend `read`, outside any frame, so they establish
  one through `Shell::run_turn` (`framed_turn_request`, a `Program::Hook`
  turn) ([[decisions/260618_run-turn-host-loop|run-turn-host-loop]]). The
  `FramedHook` carries the per-hook policy: the terminal authority (`Leased`
  for keybinding dispatch, so an `_ed-tui` body can foreground a captured
  pipeline; `Denied` elsewhere), a `kind` label for the turn's root context,
  and an optional budget arming the turn's wall. Plugins run with the host
  authority (`Capabilities::root()`) the framed turn already carries.

## Source-mapped faults and the buffer-change breaker

A hook fault resolves to the right line of the plugin file and renders with a
source arrow, exactly as a command fault does. The plugin's file source rides on
the `LoadedPlugin` as an `Arc<str>` and is installed as the framed turn's root
context, so a `Break::Error` reads against the right `FileId`.

- **The fault is rendered inside `call_plugin_hook`, while that turn's source
  registry is still live.** The next framed turn resets `shell.sources()`, so a
  deferred raw `Error` would resolve against the wrong registry. The finished
  string rides out on `HookResult.rendered_error`, deferred through
  `defer_plugin_message` past the readline line-erase escape and flushed by
  `flush_pending_messages` ([[map/repl/frontend|frontend]]). Lifecycle hooks,
  rendered in-frame, defer nothing and report via `errfmt::plugin_error`.
- **A buffer-change hook fires on every keystroke, so a slow or faulting one is
  braked.** `HookHealth` (per plugin, in `plugin.rs`) is a circuit breaker:
  `BUFFER_CHANGE_FAULT_LIMIT` consecutive faults, or any turn overrunning the
  `BUFFER_CHANGE_BUDGET` keystroke wall, trips it and the hook is skipped for the
  rest of the session; a success resets the count. The trip emits one disable
  notice (`format_plugin_disabled`). The wall is cooperative — the trampoline
  polls cancellation each reduction step, so an overrunning handler is preempted.

## The `_ed-*` builtins

`plugin_ed_builtins.rs` defines `ED_BUILTINS`, the line-editor builtin family,
one `BuiltinEntry` per op so the typechecker sees each return type and arity is
fixed per op ([[invariants/fixed-arity|fixed-arity]]). The `_` prefix hides them
from `help`. They split into reads (`_ed-get`, `_ed-text`, `_ed-cursor`,
`_ed-keymap`, `_ed-lbuffer`), buffer writes (`_ed-set`, `_ed-set-lbuffer`,
`_ed-insert`, `_ed-push`, `_ed-accept`), display effects (`_ed-ghost`,
`_ed-highlight`), services (`_ed-tui`, `_ed-history`, `_ed-parse`, `_ed-state`),
and terminal escapes (`_ed-clipboard` for OSC 52, `_ed-hyperlink` for OSC 8).
Every op requires an active `PluginContext`, else it fails with a "no plugin
context" error. They ride the REPL's `HostSurface` into `boot_shell` at
`Session::boot` ([[map/repl/loop|loop]]), so the typechecker reads them off
the session's own builtin table from the first rc check.

## Context and editor state

`plugin_editor.rs` holds the runtime types. `PluginContext` is set on `Shell`
before each hook/keybinding call and splits its data flow explicitly: `inputs`
(history, `in_readline`), `outputs` (ghost text, highlight spans, pushed
buffer, accept flag), the live `editor_state`, and a per-plugin `state_cell`.
All cursor offsets here are **character** offsets; core's
`text::char_to_byte` / `text::byte_to_char` convert at the rustyline
boundary so plugin code never handles UTF-8.

## Runtime, manifests, loading

`plugin.rs::PluginRuntime` is the `Arc<Mutex<…>>` threaded between the loop,
rustyline's `Hinter`/`Highlighter` callbacks, the structural surface's per-tick
loop, and keybinding dispatch. It holds the canonical plugin list, the
`KeyRouter`, and the `keybindings_dirty` flag directly and partitions the rest into `EditorHooks`,
`Keybindings`, and `DeferredDiagnostics` so each call site reaches only its
slice. The load-bearing rule: editor callbacks may hold the mutex, the
evaluator must not — every hook releases the lock before running ral code so
re-entrant `_ed-*` calls can re-acquire it.

The frontend-neutral key vocabulary lives in `plugin/router.rs`:
`parse_key_notation` yields a `KeyChord`/`KeyName` that rustyline adapts to its
`KeyEvent` (`chord_to_key_event`) while the structural surface matches
crossterm's against it; `Keymap` (`Emacs` / `Vi`) reduces rustyline's
`EditMode`. **Keybinding dispatch is one ordered router**: `KeyRouter` — held
on the runtime, rebuilt by `keybindings_changed` whenever the plugin list
changes — flattens every binding in load order (manifest order within a
plugin), and `resolve` returns the first entry whose chord matches and whose
`guard` regex (matched against the text left of the cursor) allows.
`Resolution::Claimed` names the owning plugin and binding index;
`Resolution::Default` is the editor's built-in tail, which each backend
realises natively (rustyline's per-chord `RouterKeyHandler` returns `None`,
the structural surface falls into its own key arms) — precedence is decided
once, so the frontends cannot disagree.

- `plugin/manifest.rs` — a manifest is the Map a plugin's top-level block
  returns; the parser extracts hook handlers (`pre-exec`, `post-exec`, `chpwd`,
  `prompt`, `buffer-change`), keybindings, and alias thunks into a
  `LoadedPlugin`. A `KeyBinding` is fully validated at load — chord parsed,
  optional `guard:` regex compiled: `ctrl-c`/`ctrl-d` are reserved outright
  (`reserved_action`, the session's escape hatches), and an unguarded binding
  on a chord carrying a ral-owned built-in action (`builtin_action` — every
  unmodified key except F1–F12) is a load error, so same-chord bindings compose
  as an ordered match with the built-in as the final arm. A `capabilities:` key
  is a load error, not silent confinement
  — plugins run with host authority; to attenuate, wrap the invocation in
  `grant { … }`.
- `plugin/load.rs` — resolves a plugin under `~/.config/ral/plugins/` or
  `RAL_PATH`, typechecks and evaluates it, instantiates a parameterised plugin
  block through a framed hook turn, registers its hooks into the shell's hook
  table (`register_plugin_hooks`), installs alias bindings, and records it
  (retaining the file source on the `LoadedPlugin`). Registration is
  reversible: hooks and aliases are committed only after every validation
  passes, so a rejected load rolls back cleanly; unloading is the exact
  inverse, unregistering the plugin's hooks and keybindings and undoing the
  env installation.
- `keybinding.rs` — when a plugin-registered key fires, rustyline stashes a
  `PendingKeybinding` and accepts the line; `dispatch_keybinding` then runs the
  handler outside the readline borrow under `HookFraming::Framed` with `Leased`
  terminal, resolving the owning plugin by **name**
  (`PluginRuntime::resolve_keybinding`) and loading/saving its `state_cell`
  exactly as the buffer-change path does. A `PendingKeybinding` carries the
  plugin's name plus a binding index *within* it, never a position in the
  runtime `Vec` — `unload_plugin` compacts that vector, so a runtime index
  would address the wrong plugin after an unload; a resolution miss re-edits
  the line unchanged. `sync_plugins` reconciles rustyline's binding table by
  full unbind-then-rebind, registering one `RouterKeyHandler` per distinct
  bound chord (`bound_chords`) that consults the live router on each press; a
  frontend matching keys itself snapshots the `KeyRouter` instead. Loading also
  runs the shadow lint: a binding `dead_entries` flags (an earlier unguarded
  entry owns its chord) is warned about, not rejected.

## Captured session commands

`host_handlers.rs::build` returns the six captured builtins the REPL's
`HostSurface` installs at boot, each closing over the shared `Arc<Mutex<…>>`
state: `jobs`, `fg`, `bg`, `disown` ([[map/repl/jobs|jobs]]), and
`load-plugin` / `unload-plugin`. They are captured rather than static because
they mutate long-lived runtime state the static descriptor cannot reach.
