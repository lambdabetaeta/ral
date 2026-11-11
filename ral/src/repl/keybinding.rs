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
use ral_core::data::FOValue;
use ral_core::data::datum::Datum as _;
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
