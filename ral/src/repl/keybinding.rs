//! Plugin keybinding dispatch.
//!
//! When a plugin-registered key fires during readline, rustyline stores a
//! [`PendingKeybinding`] and immediately accepts the line.  The REPL loop
//! then calls [`dispatch_keybinding`] to run the handler outside the
//! readline borrow, with a fresh editor context reflecting the current
//! editor state.  The handler may mutate the buffer, accept the line, or
//! push a new buffer onto the stack.  The handler receives the record
//! `[key: Str, mode: Str]`: the bound notation, and the editor's input mode
//! at the press (`insert`, `command` or `replace`).

use ral_core::HookName;
use ral_core::carrier::Transport;
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum as _;
use std::sync::Arc;

use super::frontend::EditBuffer;
use super::host::{Outcome, ReplHost};
use super::plugin::editor::{EditorState, PluginContext};
use super::plugin::{Keymap, PendingKeybinding, defer_plugin_message, lock};
use ral_core::text::byte_to_char;

/// Outcome of running a plugin keybinding handler.
///
/// In both variants the cursor is a character offset (`EditBuffer`'s unit);
/// the frontend converts back to bytes at the rustyline boundary.
pub(super) enum KeybindingOutcome {
    /// The handler called `_ed-accept`; execute this input.
    Accept(String),
    /// Return to readline with this buffer state.
    Edit(String, usize),
}

/// Execute a pending keybinding handler with the current editor state, and
/// decide from the context it leaves whether to accept or re-edit the line.
pub(super) fn dispatch_keybinding(
    pk: &PendingKeybinding,
    current: &str,
    t: &dyn Transport,
    host: &Arc<ReplHost>,
    keymap: Keymap,
) -> KeybindingOutcome {
    let unchanged = || KeybindingOutcome::Edit(current.to_string(), current.chars().count());
    // Resolve the owning plugin by name, not by position: a stale binding's
    // index could address whatever plugin now occupies that slot.  A miss
    // (the plugin was unloaded between keypress and dispatch) is benign —
    // the line re-edits unchanged, and the sequence is unbound on the next
    // `sync_plugins`.
    let resolved = {
        let rt = lock(&host.runtime);
        rt.resolve_keybinding(&pk.plugin, pk.binding_idx)
            .map(|key| (key, rt.state_cell(&pk.plugin), rt.hooks.history.clone()))
    };
    let Some((key, state_cell, history)) = resolved else {
        return unchanged();
    };

    // The plugin's persistent cell rides the context and is saved back, so a
    // handler's `_ed-state` survives between keypresses.
    let ctx = PluginContext {
        // rustyline supplied `pk.cursor_byte` in bytes; the plugin surface
        // speaks chars throughout.
        editor_state: EditorState::new(
            current.to_string(),
            byte_to_char(current, pk.cursor_byte),
            keymap,
        ),
        history,
        state_cell,
        ..PluginContext::default()
    };
    let hr = host.run_hook(
        t,
        HookName::plugin(pk.plugin.clone(), format!("key#{}", pk.binding_idx)),
        vec![FOValue::Map {
            entries: vec![
                ("key".into(), key.encode()),
                ("mode".into(), pk.mode.to_owned().encode()),
            ],
        }],
        None,
        Some(ctx),
    );

    if let Outcome::Faulted(fault) | Outcome::Walled(fault) | Outcome::Stopped(fault) = hr.outcome {
        // Deferred: the REPL loop is about to emit `\x1b[A\r\x1b[K` to erase
        // rustyline's stray newline, which would clobber an immediate
        // `eprintln!` on that very line.  Flushed afterward.
        defer_plugin_message(&host.runtime, fault);
    }
    let Some(ctx) = hr.ctx else {
        return unchanged();
    };
    let mut rt = lock(&host.runtime);
    rt.write_back_state_cell(&pk.plugin, ctx.state_cell);
    if let Some((text, cursor)) = ctx.outputs.pushed_buffer {
        rt.keybindings.buffers.push(EditBuffer { text, cursor });
    }
    drop(rt);
    let EditBuffer { text, cursor } = ctx.editor_state.into();
    if ctx.outputs.accept_line {
        KeybindingOutcome::Accept(text)
    } else {
        KeybindingOutcome::Edit(text, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::super::plugin::PluginRuntime;
    use super::super::plugin::manifest::{KeySpec, LoadedPlugin, Manifest};

    /// A plugin record carrying one keybinding under `key`, so resolution
    /// can be checked by the key it yields.
    fn plugin(name: &str, key: &str) -> LoadedPlugin {
        LoadedPlugin::admit(Manifest {
            name: name.to_string(),
            hooks: Vec::new(),
            keybindings: vec![KeySpec {
                key: key.to_string(),
                guard: None,
            }],
        })
        .expect("test key binds")
    }

    /// J3 regression: after `unload_plugin` compacts the runtime `Vec`, a
    /// keybinding still flagged for the unloaded plugin must not dispatch
    /// to whichever plugin slid into its old slot — name lookup either hits
    /// the right plugin or misses entirely.
    #[test]
    fn stale_keybinding_does_not_resolve_to_a_different_plugin() {
        let mut rt = PluginRuntime::default();
        rt.plugins.push(plugin("a", "ctrl-t"));
        rt.plugins.push(plugin("b", "ctrl-r"));

        // Before unload, "a"'s binding resolves.
        assert_eq!(rt.resolve_keybinding("a", 0), Some("ctrl-t".into()));

        // `unload_plugin` removes "a"; "b" shifts down to slot 0: the
        // exact compaction that index-based dispatch would mishandle.
        rt.plugins.remove(0);

        // The stale "a" binding now misses; it must NOT pick up "b"'s
        assert_eq!(rt.resolve_keybinding("b", 0), Some("ctrl-r".into()));
        assert_eq!(rt.resolve_keybinding("a", 0), None);
    }

    /// A binding index past the named plugin's keybinding list misses
    /// rather than panicking.
    #[test]
    fn out_of_range_binding_index_misses() {
        let mut rt = PluginRuntime::default();
        rt.plugins.push(plugin("a", "ctrl-t"));
        assert_eq!(rt.resolve_keybinding("a", 1), None);
    }
}
