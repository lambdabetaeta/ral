//! Plugin keybinding dispatch.
//!
//! When a plugin-registered key fires during readline, rustyline stores a
//! [`PendingKeybinding`] and immediately accepts the line.  The REPL loop
//! then calls [`dispatch_keybinding`] to run the handler outside the
//! readline borrow, with a fresh [`PluginContext`] reflecting the current
//! editor state.  The handler may mutate the buffer, accept the line, or
//! push a new buffer onto the stack.

use ral_core::types::PluginContext;
use ral_core::{Shell, EvalSignal};
use rustyline::config::EditMode;
use std::sync::{Arc, Mutex};

use super::plugin::{PendingKeybinding, PluginRuntime, defer_plugin_error, keymap_name, lock};

/// Outcome of running a plugin keybinding handler.
pub(super) enum KeybindingOutcome {
    /// The handler called `_editor 'accept-line'`; execute this input.
    Accept(String),
    /// Return to readline with this buffer state.
    Edit(String, usize),
}

/// Execute a pending keybinding handler with the current editor state.
///
/// Looks up the handler and capabilities from the plugin runtime, builds
/// a [`PluginContext`], calls the handler, and inspects the resulting
/// context to decide whether to accept or re-edit the line.
pub(super) fn dispatch_keybinding(
    pk: PendingKeybinding,
    current: &str,
    shell: &mut Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
    edit_mode: EditMode,
) -> KeybindingOutcome {
    let handler_and_capabilities = {
        let rt = lock(runtime);
        rt.plugins.get(pk.plugin_idx).and_then(|p| {
            p.keybindings
                .get(pk.binding_idx)
                .map(|(_, h)| (h.clone(), p.capabilities.clone()))
        })
    };
    let Some((handler, capabilities)) = handler_and_capabilities else {
        return KeybindingOutcome::Edit(current.to_string(), current.len());
    };

    let cursor_chars = current
        .char_indices()
        .take_while(|(byte_idx, _)| *byte_idx < pk.cursor_byte)
        .count();

    shell.repl.plugin_context = Some(PluginContext {
        editor_state: ral_core::types::EditorState {
            text: current.to_string(),
            cursor: cursor_chars,
            keymap: keymap_name(edit_mode).into(),
        },
        inputs: ral_core::types::PluginInputs {
            history_entries: lock(runtime).history_entries.clone(),
            in_readline: false,
        },
        outputs: ral_core::types::PluginOutputs::default(),
        state_cell: None,
        state_default_used: false,
        in_tui: false,
    });

    let result = shell.with_capabilities(capabilities, |shell| {
        ral_core::evaluator::call_value_pub(&handler, &[], shell)
    });

    if let Err(EvalSignal::Error(e)) = &result {
        let name = lock(runtime)
            .plugins
            .get(pk.plugin_idx)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        // Defer printing: the REPL loop is about to emit `\x1b[A\r\x1b[K`
        // to erase rustyline's stray newline, which would clobber an
        // immediate `eprintln!` on that very line.  Flushed afterward.
        defer_plugin_error(runtime, &name, "keybinding handler failed", e);
    }

    let Some(ctx) = shell.repl.plugin_context.take() else {
        return KeybindingOutcome::Edit(current.to_string(), current.len());
    };

    if let Some(pushed) = ctx.outputs.pushed_buffer {
        lock(runtime).buffer_stack.push(pushed);
    }

    if ctx.outputs.accept_line {
        KeybindingOutcome::Accept(ctx.editor_state.text)
    } else {
        KeybindingOutcome::Edit(ctx.editor_state.text, ctx.editor_state.cursor)
    }
}
