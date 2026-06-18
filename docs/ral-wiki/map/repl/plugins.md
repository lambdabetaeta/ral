---
generated_at_commit: 7ba500b
generated_at_date: 2026-06-17
covers_paths: [ral/src/repl/plugin.rs, ral/src/repl/plugin/, ral/src/repl/plugin_editor.rs, ral/src/repl/plugin_ed_builtins.rs, ral/src/repl/keybinding.rs, ral/src/repl/host_handlers.rs]
---

# Map: repl / plugins

The editing surface plugins program against. It lives entirely in the `ral`
crate because editor state is a host concern; core stores the context
type-erased as `Box<dyn Any>` in `ReplScratch.plugin_context` and never inspects
it ([[decisions/260514_repl-builtins-stay-in-repl|repl-builtins-stay-in-repl]]).

## The `_ed-*` builtins

`plugin_ed_builtins.rs` defines `ED_BUILTINS`, the line-editor builtin family,
one `BuiltinEntry` per op so the typechecker sees each return type and arity is
fixed per op ([[invariants/fixed-arity|fixed-arity]]). The `_` prefix hides them from
`help`. They split into reads (`_ed-get`, `_ed-text`, `_ed-cursor`,
`_ed-keymap`, `_ed-lbuffer`), buffer writes (`_ed-set`, `_ed-set-lbuffer`,
`_ed-insert`, `_ed-push`, `_ed-accept`), display effects (`_ed-ghost`,
`_ed-highlight`), services (`_ed-tui`, `_ed-history`, `_ed-parse`, `_ed-state`),
and terminal escapes (`_ed-clipboard` for OSC 52, `_ed-hyperlink` for OSC 8).
Every op requires an active `PluginContext`, else it fails with a "no plugin
context" error. `register_host_surface` publishes them into core's static host
table at process start; `Session::boot` also installs them into the session's
own builtin table.

## Context and editor state

`plugin_editor.rs` holds the runtime types. `PluginContext` is set on `Shell`
before each hook/keybinding call and splits its data flow explicitly: `inputs`
(history, `in_readline`), `outputs` (ghost text, highlight spans, pushed
buffer, accept flag), the live `editor_state`, and a per-plugin `state_cell`.
All cursor offsets here are **character** offsets; `char_to_byte` / `byte_to_char`
convert at the rustyline boundary so plugin code never handles UTF-8.

## Runtime, manifests, loading

`plugin/mod.rs::PluginRuntime` is the `Arc<Mutex<…>>` threaded between the
loop, rustyline's `Hinter`/`Highlighter` callbacks, and keybinding dispatch.
It is partitioned into `PluginSnapshot`, `EditorHooks`, `Keybindings`, and
`DeferredDiagnostics` so each call site reaches only its slice. The load-bearing
rule: editor callbacks may hold the mutex, the evaluator must not — every hook
releases the lock before running ral code so re-entrant `_ed-*` calls can
re-acquire it.

- `plugin/manifest.rs` — a manifest is the Map a plugin's top-level block
  returns; the parser extracts hook handlers (`pre-exec`, `post-exec`, `chpwd`,
  `prompt`, `buffer-change`), keybindings, and alias thunks into a
  `LoadedPlugin`.
- `plugin/load.rs` — resolves a plugin under `~/.config/ral/plugins/` or
  `RAL_PATH`, evaluates it under a `ScriptContextGuard`, installs alias
  bindings, and records it. Unloading reverses the env installation.
- `keybinding.rs` — when a plugin-registered key fires, rustyline stashes a
  `PendingKeybinding` and accepts the line; `dispatch_keybinding` then runs the
  handler outside the readline borrow, resolving the owning plugin by name
  (`PluginRuntime::resolve_keybinding`) and loading/saving its `state_cell`
  exactly as the buffer-change path does. A `PendingKeybinding` carries the
  plugin's **name**, not its position in the runtime `Vec` — `unload_plugin`
  compacts that vector, so an index would address the wrong plugin after an
  unload. `sync_plugins` reconciles rustyline's binding table by full
  unbind-then-rebind, dropping the sequences a removed plugin owned.

## Captured session commands

`host_handlers.rs::build` returns six captured builtins installed at boot, each
closing over the shared `Arc<Mutex<…>>` state: `jobs`, `fg`, `bg`, `disown`
([[map/repl/jobs|jobs]]), and `load-plugin` / `unload-plugin`. They are captured
rather than static because they mutate long-lived runtime state the static
descriptor cannot reach.
