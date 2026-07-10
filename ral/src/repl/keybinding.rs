//! Plugin keybinding dispatch.
//!
//! When a plugin-registered key fires during readline, rustyline stores a
//! [`PendingKeybinding`] and immediately accepts the line.  The REPL loop
//! then calls [`dispatch_keybinding`] to run the handler outside the
//! readline borrow, with a fresh [`PluginContext`] reflecting the current
//! editor state.  The handler may mutate the buffer, accept the line, or
//! push a new buffer onto the stack.

use ral_core::types::Capabilities;
use ral_core::{HookName, RequestedTerminalAccess, Shell, Value};
use std::sync::{Arc, Mutex};

use super::frontend::EditBuffer;
use super::plugin::{
    FramedHook, HookFor, HookFraming, Keymap, PendingKeybinding, PluginRuntime, call_plugin_hook,
    defer_plugin_message, keymap_name, lock,
};
use super::plugin_editor::{EditorState, PluginContext, PluginInputs, PluginOutputs, byte_to_char};

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

/// Character offset of the end of `s`.  Used on early-return paths so
/// the resulting `EditBuffer`/`KeybindingOutcome::Edit` carries a unit-
/// correct cursor (chars), not a byte length.
fn end_of(s: &str) -> usize {
    s.chars().count()
}

/// Execute a pending keybinding handler with the current editor state.
///
/// Looks up the handler from the plugin runtime, builds a
/// [`PluginContext`], invokes the hook through [`call_plugin_hook`], and
/// inspects the resulting context to decide whether to accept or re-edit
/// the line.
pub(super) fn dispatch_keybinding(
    pk: PendingKeybinding,
    current: &str,
    shell: &mut Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
    keymap: Keymap,
) -> KeybindingOutcome {
    // Resolve the owning plugin by name, not by position: the index a
    // stale binding once carried would now address whatever plugin slid
    // into that slot.  A miss (the plugin was unloaded between keypress
    // and dispatch) is benign — the line re-edits unchanged, and the
    // sequence is unbound on the next `sync_plugins`.
    let resolved = {
        let rt = lock(runtime);
        rt.resolve_keybinding(&pk.plugin, pk.binding_idx)
            .map(|(idx, key_str, handler)| {
                (
                    idx,
                    key_str,
                    rt.plugins[idx].state_cell.clone(),
                    rt.plugins[idx].source.clone(),
                    handler,
                )
            })
    };
    let Some((idx, key_str, state_cell, _source, _handler)) = resolved else {
        return KeybindingOutcome::Edit(current.to_string(), end_of(current));
    };

    let hook = HookName::plugin(pk.plugin.clone(), format!("key:{key_str}"));

    // rustyline supplied `pk.cursor_byte` in bytes; convert once for the
    // plugin surface (which speaks chars throughout).
    let cursor_chars = byte_to_char(current, pk.cursor_byte);

    // Load the plugin's persistent cell into the context and save any
    // mutation back afterwards, mirroring `run_buffer_change_hooks`; a
    // keybinding handler's `_ed-state` must survive between keypresses.
    let state_loaded = state_cell.is_some();
    let ctx_in = PluginContext {
        editor_state: EditorState {
            text: current.to_string(),
            cursor: cursor_chars,
            keymap: keymap_name(keymap).into(),
        },
        inputs: PluginInputs {
            history_entries: lock(runtime).hooks.history.clone(),
            in_readline: false,
        },
        outputs: PluginOutputs::default(),
        state_cell: state_cell.clone(),
        state_default_used: state_loaded,
    };

    #[allow(
        clippy::cast_possible_wrap,
        reason = "cursor char offset, far below i64::MAX"
    )]
    let cursor = cursor_chars as i64;
    let hr = call_plugin_hook(
        shell,
        HookFor { name: &pk.plugin },
        &hook,
        &[Value::map(vec![
            ("line".into(), Value::String(current.to_string())),
            ("cursor".into(), Value::Int(cursor)),
            (
                "history".into(),
                Value::List(
                    lock(runtime)
                        .hooks
                        .history
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            ),
            (
                "keymap".into(),
                Value::String(keymap_name(keymap).to_string()),
            ),
            ("state".into(), state_cell.clone().unwrap_or(Value::Unit)),
        ])],
        Some(ctx_in),
        HookFraming::Framed(FramedHook {
            terminal: RequestedTerminalAccess::Leased,
            kind: "keybinding",
            caps: Capabilities::root(),
            budget: None,
        }),
    );

    if let Some(rendered) = &hr.rendered_error {
        // Defer printing: the REPL loop is about to emit `\x1b[A\r\x1b[K`
        // to erase rustyline's stray newline, which would clobber an
        // immediate `eprintln!` on that very line.  Flushed afterward.
        defer_plugin_message(runtime, rendered.clone());
    }

    let Some(ctx) = hr.ctx else {
        return KeybindingOutcome::Edit(current.to_string(), end_of(current));
    };

    {
        let mut rt = lock(runtime);
        if let Some(p) = rt.plugins.get_mut(idx) {
            p.state_cell.clone_from(&ctx.state_cell);
        }
    }

    if let Some((text, cursor)) = ctx.outputs.pushed_buffer {
        lock(runtime)
            .keybindings
            .buffers
            .push(EditBuffer { text, cursor });
    }

    if ctx.outputs.accept_line {
        KeybindingOutcome::Accept(ctx.editor_state.text)
    } else {
        KeybindingOutcome::Edit(ctx.editor_state.text, ctx.editor_state.cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::super::plugin::PluginRuntime;
    use super::super::plugin::manifest::LoadedPlugin;
    use ral_core::Value;
    use std::collections::HashMap;

    /// A plugin record carrying one keybinding whose handler is the given
    /// marker value, so resolution can be checked by identity of the
    /// handler it returns.
    fn plugin(name: &str, key: &str, handler: Value) -> LoadedPlugin {
        LoadedPlugin {
            name: name.to_string(),
            hooks: HashMap::new(),
            keybindings: vec![(key.to_string(), handler)],
            bindings: Vec::new(),
            state_cell: None,
            source: std::sync::Arc::from(""),
            buffer_change_health: crate::repl::plugin::HookHealth::default(),
        }
    }

    /// J3 regression: after `unload_plugin` compacts the runtime `Vec`, a
    /// keybinding still flagged for the unloaded plugin must not dispatch
    /// to whichever plugin slid into its old slot — name lookup either hits
    /// the right plugin or misses entirely.
    #[test]
    fn stale_keybinding_does_not_resolve_to_a_different_plugin() {
        let mut rt = PluginRuntime::default();
        rt.plugins.push(plugin("a", "ctrl-t", Value::Int(1)));
        rt.plugins.push(plugin("b", "ctrl-r", Value::Int(2)));

        // Before unload, "a"'s binding resolves to slot 0 with handler 1.
        assert_eq!(
            rt.resolve_keybinding("a", 0),
            Some((0, "ctrl-t".into(), Value::Int(1)))
        );

        // `unload_plugin` removes "a"; "b" shifts down to slot 0 — the
        // exact compaction that the old index-keyed dispatch mishandled.
        rt.plugins.remove(0);

        // The stale "a" binding now misses; it must NOT pick up "b"'s
        assert_eq!(
            rt.resolve_keybinding("b", 0),
            Some((0, "ctrl-r".into(), Value::Int(2)))
        );
        assert_eq!(rt.resolve_keybinding("a", 0), None);
    }

    /// A binding index past the named plugin's keybinding list misses
    /// rather than panicking.
    #[test]
    fn out_of_range_binding_index_misses() {
        let mut rt = PluginRuntime::default();
        rt.plugins.push(plugin("a", "ctrl-t", Value::Int(1)));
        assert_eq!(rt.resolve_keybinding("a", 1), None);
    }
}
