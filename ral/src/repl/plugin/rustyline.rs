//! The rustyline boundary of the plugin runtime.
//!
//! Everything above this file (the hook primitive, the circuit breaker, the
//! buffer-change driver) is frontend-neutral; this module is where the
//! [`KeyChord`]/[`Resolution`] vocabulary and the runtime mutex meet
//! rustyline's own event and key types. Precedence and dispatch stay decided
//! once, in the router; this module does only the translation.

use std::sync::{Arc, Mutex};

use rustyline::history::{DefaultHistory, History};
use rustyline::{
    Cmd, ConditionalEventHandler, Editor, Event, EventContext, InputMode, KeyCode, KeyEvent,
    Modifiers, Movement, RepeatCount,
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

/// The one rustyline handler per distinct bound sequence.  It consults the
/// live router, so several entries on one sequence resolve in table order;
/// `None` on [`Resolution::Default`] runs rustyline's own action for the
/// key.  Resolution is pure — the evaluator never runs here; a claimed
/// binding is stashed as pending and dispatched after readline returns.
struct RouterKeyHandler {
    runtime: Arc<Mutex<PluginRuntime>>,
    chords: Vec<KeyChord>,
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
        match rt.router.resolve(&self.chords, ctx.line(), ctx.pos()) {
            Resolution::Claimed {
                plugin,
                binding_idx,
            } => {
                rt.keybindings.pending = Some(PendingKeybinding {
                    plugin,
                    binding_idx,
                    cursor_byte: ctx.pos(),
                    mode: match ctx.input_mode() {
                        InputMode::Insert => "insert",
                        InputMode::Command => "command",
                        InputMode::Replace => "replace",
                    },
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
    let mods = [
        (chord.ctrl, Modifiers::CTRL),
        (chord.alt, Modifiers::ALT),
        (chord.shift, Modifiers::SHIFT),
    ]
    .into_iter()
    .filter_map(|(on, m)| on.then_some(m))
    .collect();
    KeyEvent(code, mods)
}

/// Reconcile plugin keybindings with rustyline when `load` / `unload`
/// has touched the plugin list.  The `keybindings_dirty` flag is set by
/// those routines and cleared here; if it isn't set we skip the work.
///
/// rustyline owns its binding table but the router owns precedence, so
/// registration is one [`RouterKeyHandler`] per distinct bound sequence:
/// every event in `bound` (the last sync's) is dropped, then the router's
/// sequences are re-bound as `Event::KeySeq`.  A plugin removed by `unload`
/// therefore loses its sequences here rather than leaving a handler behind.
pub(in crate::repl) fn sync_plugins(
    runtime: &Arc<Mutex<PluginRuntime>>,
    rl: &mut Editor<RalHelper, DefaultHistory>,
    bound: &mut Vec<Event>,
) {
    let keys = {
        let mut rt = lock(runtime);
        if !rt.keybindings_dirty {
            return;
        }
        rt.keybindings_dirty = false;
        rt.router.bound_keys()
    };
    for event in bound.drain(..) {
        rl.unbind_sequence(event);
    }
    *bound = keys
        .into_iter()
        .map(|chords| {
            let event = Event::KeySeq(chords.iter().map(|&c| chord_to_key_event(c)).collect());
            rl.bind_sequence(
                event.clone(),
                rustyline::EventHandler::Conditional(Box::new(RouterKeyHandler {
                    runtime: runtime.clone(),
                    chords,
                })),
            );
            event
        })
        .collect();
}

/// Snapshot rustyline history (most-recent-first) into the runtime so plugin
/// hooks can read it via `_ed-history`.
pub(in crate::repl) fn snapshot_history(
    rl: &Editor<RalHelper, DefaultHistory>,
    runtime: &Arc<Mutex<PluginRuntime>>,
) {
    let entries: Arc<[String]> = (0..rl.history().len())
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

    /// The rustyline boundary: a parsed notation adapts to exactly the
    /// `KeyEvent`s rustyline binds against, so `sync_plugins` binds exactly
    /// the notation the router resolves.
    #[test]
    fn chords_adapt_to_rustyline_key_events() {
        let ev = |s| {
            parse_key_notation(s).map(|chords| {
                chords
                    .into_iter()
                    .map(chord_to_key_event)
                    .collect::<Vec<_>>()
            })
        };
        assert_eq!(
            ev("ctrl-r"),
            Ok(vec![KeyEvent(KeyCode::Char('r'), Modifiers::CTRL)])
        );
        assert_eq!(
            ev("alt-c"),
            Ok(vec![KeyEvent(KeyCode::Char('c'), Modifiers::ALT)])
        );
        assert_eq!(ev("tab"), Ok(vec![KeyEvent(KeyCode::Tab, Modifiers::NONE)]));
        assert_eq!(ev("f5"), Ok(vec![KeyEvent(KeyCode::F(5), Modifiers::NONE)]));
        assert_eq!(
            ev("shift-f5"),
            Ok(vec![KeyEvent(KeyCode::F(5), Modifiers::SHIFT)])
        );
        assert_eq!(
            ev("ctrl-x ctrl-e"),
            Ok(vec![
                KeyEvent(KeyCode::Char('x'), Modifiers::CTRL),
                KeyEvent(KeyCode::Char('e'), Modifiers::CTRL)
            ])
        );
    }
}
