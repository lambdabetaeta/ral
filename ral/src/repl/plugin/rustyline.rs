//! The rustyline boundary of the plugin runtime.
//!
//! Everything above this file (the hook primitive, the circuit breaker, the
//! buffer-change driver) is frontend-neutral; this module is where the
//! [`KeyChord`]/[`Resolution`] vocabulary and the runtime mutex meet
//! rustyline's own event and key types. The structural frontend adapts the
//! same vocabulary at its own boundary instead (`frontend::structural`), so
//! precedence and dispatch stay decided once, in the router, while each
//! backend does only its own translation here.

use std::sync::{Arc, Mutex};

use rustyline::history::{DefaultHistory, History};
use rustyline::{
    Cmd, ConditionalEventHandler, Editor, Event, EventContext, KeyCode, KeyEvent, Modifiers,
    Movement, RepeatCount,
};

use super::super::complete::RalHelper;
use super::{KeyChord, KeyName, PendingKeybinding, PluginRuntime, Resolution, lock};

/// Ctrl-D: EOF on an empty line; delete-char otherwise.
/// Overrides rustyline's Vi-mode default of submitting the line, matching
/// the bash/zsh convention in every edit mode.
pub(in crate::repl) struct CtrlDHandler;

impl ConditionalEventHandler for CtrlDHandler {
    fn handle(
        &self,
        _evt: &Event,
        n: RepeatCount,
        positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        if ctx.line().is_empty() {
            Some(Cmd::EndOfFile)
        } else {
            Some(Cmd::Kill(if positive {
                Movement::ForwardChar(n)
            } else {
                Movement::BackwardChar(n)
            }))
        }
    }
}

/// The one rustyline handler per distinct bound chord.  It consults the
/// live router, so several entries on one chord resolve in table order;
/// `None` on [`Resolution::Default`] runs rustyline's own action for the
/// key.  Resolution is pure — the evaluator never runs here; a claimed
/// binding is stashed as pending and dispatched after readline returns.
struct RouterKeyHandler {
    runtime: Arc<Mutex<PluginRuntime>>,
    chord: KeyChord,
}

impl ConditionalEventHandler for RouterKeyHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext,
    ) -> Option<Cmd> {
        let mut rt = lock(&self.runtime);
        match rt.router.resolve(self.chord, ctx.line(), ctx.pos()) {
            Resolution::Claimed {
                plugin,
                binding_idx,
            } => {
                rt.keybindings.pending = Some(PendingKeybinding {
                    plugin,
                    binding_idx,
                    cursor_byte: ctx.pos(),
                });
                drop(rt);
                Some(Cmd::AcceptLine)
            }
            Resolution::Default => None,
        }
    }
}

/// Adapt a [`KeyChord`] to the `KeyEvent` rustyline binds against.
/// [`sync_plugins`] is the only caller.
fn chord_to_key_event(chord: KeyChord) -> KeyEvent {
    let code = match chord.name {
        KeyName::Char(c) => KeyCode::Char(c),
        KeyName::Tab => KeyCode::Tab,
        KeyName::Enter => KeyCode::Enter,
        KeyName::Escape => KeyCode::Esc,
        KeyName::Up => KeyCode::Up,
        KeyName::Down => KeyCode::Down,
        KeyName::Left => KeyCode::Left,
        KeyName::Right => KeyCode::Right,
        KeyName::Home => KeyCode::Home,
        KeyName::End => KeyCode::End,
        KeyName::Delete => KeyCode::Delete,
        KeyName::Backspace => KeyCode::Backspace,
        KeyName::F(n) => KeyCode::F(n),
    };
    let mut mods = Modifiers::NONE;
    if chord.ctrl {
        mods |= Modifiers::CTRL;
    }
    if chord.alt {
        mods |= Modifiers::ALT;
    }
    KeyEvent(code, mods)
}

/// Reconcile plugin keybindings with rustyline when `load` / `unload`
/// has touched the plugin list.  The `keybindings_dirty` flag is set by
/// those routines and cleared here; if it isn't set we skip the work.
///
/// rustyline owns its binding table but the router owns precedence, so
/// registration is one [`RouterKeyHandler`] per distinct bound chord:
/// every sequence bound on the last sync is dropped, then the router's
/// chords are re-bound.  A plugin removed by `unload` therefore loses
/// its sequences here rather than leaving a handler behind.
pub(in crate::repl) fn sync_plugins(
    runtime: &Arc<Mutex<PluginRuntime>>,
    rl: &mut Editor<RalHelper, DefaultHistory>,
) {
    let chords: Vec<KeyChord> = {
        let mut rt = lock(runtime);
        if !rt.keybindings_dirty {
            return;
        }
        rt.keybindings_dirty = false;
        for key in std::mem::take(&mut rt.keybindings.bound) {
            rl.unbind_sequence(key);
        }
        rt.router.bound_chords()
    };

    let mut bound = Vec::new();
    for chord in chords {
        let key_event = chord_to_key_event(chord);
        rl.bind_sequence(
            key_event,
            rustyline::EventHandler::Conditional(Box::new(RouterKeyHandler {
                runtime: runtime.clone(),
                chord,
            })),
        );
        bound.push(key_event);
    }
    lock(runtime).keybindings.bound = bound;
}

/// Snapshot rustyline history (most-recent-first) into the runtime so plugin
/// hooks can read it via `_ed-history`.
pub(in crate::repl) fn snapshot_history(
    rl: &Editor<RalHelper, DefaultHistory>,
    runtime: &Arc<Mutex<PluginRuntime>>,
) {
    let entries: Vec<String> = (0..rl.history().len())
        .rev()
        .filter_map(|i| {
            rl.history()
                .get(i, rustyline::history::SearchDirection::Forward)
                .ok()?
                .map(|e| e.entry.to_string())
        })
        .collect();
    lock(runtime).hooks.history = entries;
}

#[cfg(test)]
mod tests {
    use super::super::router::parse_key_notation;
    use super::*;

    /// The rustyline boundary: a parsed chord adapts to exactly the `KeyEvent`
    /// rustyline binds against, so `sync_plugins` and the structural matcher
    /// agree on what a notation means.
    #[test]
    fn chord_adapts_to_rustyline_key_event() {
        let ev = |s| parse_key_notation(s).map(chord_to_key_event);
        assert_eq!(
            ev("ctrl-r"),
            Some(KeyEvent(KeyCode::Char('r'), Modifiers::CTRL))
        );
        assert_eq!(
            ev("alt-c"),
            Some(KeyEvent(KeyCode::Char('c'), Modifiers::ALT))
        );
        assert_eq!(ev("tab"), Some(KeyEvent(KeyCode::Tab, Modifiers::NONE)));
        assert_eq!(ev("f5"), Some(KeyEvent(KeyCode::F(5), Modifiers::NONE)));
    }
}
