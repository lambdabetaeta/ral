//! Plugin runtime: shared mutable state threaded between the REPL loop,
//! rustyline callbacks, and plugin handler invocations.
//!
//! The `Arc<Mutex<PluginRuntime>>` lives across rustyline's `Hinter` and
//! `Highlighter` (which require `Send + Sync`) and the REPL's own
//! `dispatch_keybinding`.  Helpers here own the buffer-change hook
//! engine, the keybinding bridge, the deferred-message buffer, and the
//! lifecycle-hook fold operator.

use ral_core::types::{Capabilities, EditorState, HighlightSpan, LoadedPlugin, PluginContext};
use ral_core::{Shell, EvalSignal, Value};

use super::errfmt::{format_plugin_error, plugin_error, plugin_warning};
use rustyline::config::EditMode;
use rustyline::history::{DefaultHistory, History};
use rustyline::{
    Cmd, ConditionalEventHandler, Editor, Event, EventContext, KeyCode, KeyEvent, Modifiers,
    Movement, RepeatCount,
};
use std::sync::{Arc, Mutex, MutexGuard};

use super::complete::RalHelper;

// ── Lock helper ─────────────────────────────────────────────────────────

pub(super) fn lock(m: &Arc<Mutex<PluginRuntime>>) -> MutexGuard<'_, PluginRuntime> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Name of the keymap rustyline is in: `"viins"` for vi insert, `"emacs"` otherwise.
/// Surfaced to plugin hooks via the `_editor 'keymap'` query.
pub(super) fn keymap_name(mode: EditMode) -> &'static str {
    match mode {
        EditMode::Vi => "viins",
        _ => "emacs",
    }
}

// ── Plugin runtime ──────────────────────────────────────────────────────

/// Shared state threaded between the REPL loop, rustyline callbacks
/// (Hinter, Highlighter), and keybinding handlers.
/// Wrapped in `Arc<Mutex<>>` because rustyline requires `ConditionalEventHandler: Send + Sync`.
#[derive(Default)]
pub(super) struct PluginRuntime {
    /// Snapshotted plugin list, synced from `shell.plugins` before readline.
    pub(super) plugins: Vec<LoadedPlugin>,
    /// Ghost text from the most recent buffer-change cycle.
    pub(super) ghost_text: Option<String>,
    /// Highlight spans from all plugins, composited each cycle.
    pub(super) highlight_spans: Vec<HighlightSpan>,
    /// Previous buffer/cursor, for change detection.
    prev_buffer: String,
    prev_cursor: usize,
    /// Shell snapshot for running hooks inside rustyline callbacks.
    pub(super) hook_env: Option<Shell>,
    /// Keybinding fired during the last readline; consumed by the REPL loop.
    pub(super) pending_keybinding: Option<PendingKeybinding>,
    /// Editor state exposed to plugin hooks via `_editor`.
    pub(super) editor_state: EditorState,
    /// History snapshot for `_editor 'history'`.
    pub(super) history_entries: Vec<String>,
    /// Buffer stack from `_editor 'push'`.
    pub(super) buffer_stack: Vec<(String, usize)>,
    /// Plugin generation at last sync; avoids redundant re-registration.
    pub(super) synced_generation: usize,
    /// Plugin diagnostics produced while a readline session is active.
    /// Buffered here so the REPL loop can flush them past the
    /// `\x1b[A\r\x1b[K` line-erase that follows `Cmd::AcceptLine` —
    /// printing them immediately would land on a line that escape clobbers.
    pub(super) pending_messages: Vec<String>,
}

/// Buffer a plugin error for the REPL loop to flush after readline returns.
///
/// Use this from inside the readline loop (keybinding dispatch,
/// buffer-change hooks).  For one-shot lifecycle paths where no escape
/// sequence is pending, `errfmt::plugin_error` may be called directly.
pub(super) fn defer_plugin_error(
    runtime: &Arc<Mutex<PluginRuntime>>,
    plugin_name: &str,
    context: &str,
    err: &ral_core::types::Error,
) {
    lock(runtime)
        .pending_messages
        .push(format_plugin_error(plugin_name, context, err));
}

/// Drain and write any buffered plugin diagnostics to stderr.
///
/// Called by the REPL loop at points where the terminal is in a stable
/// state (after any line-erase escape, before the next prompt) so each
/// message lands on its own durable line above the prompt.
pub(crate) fn flush_pending_messages(runtime: &Arc<Mutex<PluginRuntime>>) {
    let msgs: Vec<String> = std::mem::take(&mut lock(runtime).pending_messages);
    for m in msgs {
        eprintln!("{m}");
    }
}

pub(super) struct PendingKeybinding {
    pub(super) plugin_idx: usize,
    pub(super) binding_idx: usize,
    /// Cursor position as a byte offset into the line at the moment the key fired.
    pub(super) cursor_byte: usize,
}

// ── Buffer-change hooks ─────────────────────────────────────────────────

/// Drive buffer-change hooks whenever the line or cursor moves.
/// Called from `Hinter::hint()`, which holds no lock; we acquire and release
/// the runtime lock around each evaluator call to avoid re-entrancy.
pub(super) fn run_buffer_change_hooks(runtime: &Arc<Mutex<PluginRuntime>>, line: &str, pos: usize) {
    // ── Phase 1: collect work items under lock, then release ─────────────
    let (old_buf, handlers, mut hook_env) = {
        let mut rt = lock(runtime);
        if line == rt.prev_buffer && pos == rt.prev_cursor {
            return;
        }
        let old_buf = std::mem::replace(&mut rt.prev_buffer, line.to_string());
        rt.prev_cursor = pos;

        let handlers: Vec<(usize, Value, Capabilities)> = rt
            .plugins
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                p.hooks
                    .get("buffer-change")
                    .map(|h| (i, h.clone(), p.capabilities.clone()))
            })
            .collect();

        if handlers.is_empty() {
            rt.ghost_text = None;
            rt.highlight_spans.clear();
            return;
        }

        let Some(hook_env) = rt.hook_env.take() else {
            return;
        };
        (old_buf, handlers, hook_env)
    }; // lock released

    // Bring editor state in hook context up to date.
    if let Some(ctx) = hook_env.repl.plugin_context.as_mut() {
        ctx.editor_state.text = line.to_string();
        ctx.editor_state.cursor = pos;
    }

    let args = [
        Value::String(old_buf),
        Value::String(line.to_string()),
        Value::Int(pos as i64),
    ];
    let mut ghost: Option<String> = None;
    let mut spans: Vec<HighlightSpan> = Vec::new();

    // ── Phase 2: run each handler with lock released around evaluator ────
    for (idx, handler, capabilities) in handlers {
        // Load per-plugin state into the hook context.
        {
            let rt = lock(runtime);
            if let Some(ctx) = hook_env.repl.plugin_context.as_mut() {
                let plugin = rt.plugins.get(idx);
                ctx.state_cell = plugin.and_then(|p| p.state_cell.clone());
                ctx.state_default_used = ctx.state_cell.is_some();
                ctx.outputs.ghost_text = None;
                ctx.outputs.highlight_spans.clear();
            }
        }

        let result = hook_env.with_capabilities(capabilities, |shell| {
            ral_core::evaluator::call_value_pub(&handler, &args, shell)
        });

        if let Err(EvalSignal::Error(e)) = &result {
            let name = lock(runtime)
                .plugins
                .get(idx)
                .map(|p| p.name.clone())
                .unwrap_or_default();
            defer_plugin_error(runtime, &name, "hook 'buffer-change' failed", e);
        }

        if let Some(ctx) = &hook_env.repl.plugin_context {
            if let Some(g) = ctx.outputs.ghost_text.clone() {
                ghost = Some(g);
            }
            spans.extend(ctx.outputs.highlight_spans.iter().cloned());
        }

        // Save plugin state cell back.
        {
            let mut rt = lock(runtime);
            if let Some(ctx) = &hook_env.repl.plugin_context
                && let Some(p) = rt.plugins.get_mut(idx)
            {
                p.state_cell = ctx.state_cell.clone();
            }
        }
    }

    // ── Phase 3: store results, return hook_env to runtime ───────────────
    let mut rt = lock(runtime);
    rt.ghost_text = ghost;
    rt.highlight_spans = spans;
    rt.hook_env = Some(hook_env);
}

// ── Keybinding handlers ─────────────────────────────────────────────────

pub(super) struct PluginKeyHandler {
    pub(super) runtime: Arc<Mutex<PluginRuntime>>,
    pub(super) plugin_idx: usize,
    pub(super) binding_idx: usize,
}

impl ConditionalEventHandler for PluginKeyHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext,
    ) -> Option<Cmd> {
        if let Ok(mut rt) = self.runtime.lock() {
            rt.pending_keybinding = Some(PendingKeybinding {
                plugin_idx: self.plugin_idx,
                binding_idx: self.binding_idx,
                cursor_byte: _ctx.pos(),
            });
        }
        Some(Cmd::AcceptLine)
    }
}

/// Ctrl-D: EOF on an empty line; delete-char otherwise.
/// Overrides rustyline's Vi-mode default of submitting the line, matching
/// the bash/zsh convention in every edit mode.
pub(super) struct CtrlDHandler;

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

/// Parse a key notation string ("ctrl-r", "alt-x", "f5", "tab", …) into a
/// rustyline `KeyEvent`.  Returns `None` for unrecognised notations.
pub(super) fn parse_key_notation(key: &str) -> Option<KeyEvent> {
    const NAMED: &[(&str, KeyCode)] = &[
        ("tab", KeyCode::Tab),
        ("enter", KeyCode::Enter),
        ("escape", KeyCode::Esc),
        ("up", KeyCode::Up),
        ("down", KeyCode::Down),
        ("left", KeyCode::Left),
        ("right", KeyCode::Right),
        ("home", KeyCode::Home),
        ("end", KeyCode::End),
        ("delete", KeyCode::Delete),
        ("backspace", KeyCode::Backspace),
    ];
    let key = key.trim();
    if key.len() == 1 {
        return Some(KeyEvent(KeyCode::Char(key.chars().next()?), Modifiers::NONE));
    }
    if let Some(&(_, code)) = NAMED.iter().find(|(n, _)| *n == key) {
        return Some(KeyEvent(code, Modifiers::NONE));
    }
    for (prefix, mods) in [("ctrl-", Modifiers::CTRL), ("alt-", Modifiers::ALT)] {
        if let Some(rest) = key.strip_prefix(prefix) {
            return Some(KeyEvent(KeyCode::Char(rest.chars().next()?), mods));
        }
    }
    let num = key.strip_prefix('f').and_then(|s| s.parse::<u8>().ok())?;
    (1..=12).contains(&num).then_some(KeyEvent(KeyCode::F(num), Modifiers::NONE))
}

// ── Plugin lifecycle helpers ─────────────────────────────────────────────

/// Sync plugins from shell to the shared runtime and re-register keybindings
/// with rustyline when the generation counter advances.
pub(super) fn sync_plugins(
    shell: &Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
    rl: &mut Editor<RalHelper, DefaultHistory>,
) {
    {
        let mut rt = lock(runtime);
        if rt.synced_generation == shell.registry.generation {
            return;
        }
        rt.plugins = shell.registry.plugins.clone();
        rt.synced_generation = shell.registry.generation;
    }

    for (pi, plugin) in shell.registry.plugins.iter().enumerate() {
        for (bi, (key_str, _)) in plugin.keybindings.iter().enumerate() {
            if let Some(key_event) = parse_key_notation(key_str) {
                rl.bind_sequence(
                    key_event,
                    rustyline::EventHandler::Conditional(Box::new(PluginKeyHandler {
                        runtime: runtime.clone(),
                        plugin_idx: pi,
                        binding_idx: bi,
                    })),
                );
            } else {
                plugin_warning(
                    &plugin.name,
                    &format!("invalid key notation '{key_str}', skipping"),
                );
            }
        }
    }
}

/// Prepare the hook shell and reset per-readline state before entering readline.
pub(super) fn prepare_hook_env(
    shell: &Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
    edit_mode: EditMode,
) {
    let mut rt = lock(runtime);

    rt.editor_state = EditorState {
        text: String::new(),
        cursor: 0,
        keymap: keymap_name(edit_mode).into(),
    };
    rt.prev_buffer.clear();
    rt.prev_cursor = 0;
    rt.ghost_text = None;
    rt.highlight_spans.clear();

    let mut hook_env = Shell::child_from(&shell.snapshot(), shell);
    hook_env.io.interactive = true;
    hook_env.repl.plugin_context = Some(PluginContext {
        editor_state: rt.editor_state.clone(),
        inputs: ral_core::types::PluginInputs {
            history_entries: rt.history_entries.clone(),
            in_readline: true,
        },
        outputs: ral_core::types::PluginOutputs::default(),
        state_cell: None,
        state_default_used: false,
        in_tui: false,
    });
    rt.hook_env = Some(hook_env);
}

/// RAII guard that releases the hook shell when dropped.  Construct after
/// `prepare_hook_env`; dropping it on any exit path clears `hook_env` so
/// subsequent buffer-change hooks bail cleanly until the next prepare.
pub(super) struct HookEnvGuard<'a>(pub(super) &'a Arc<Mutex<PluginRuntime>>);

impl Drop for HookEnvGuard<'_> {
    fn drop(&mut self) {
        lock(self.0).hook_env = None;
    }
}

/// Fold a named hook over all plugins that register it, threading an
/// accumulator through each call.  The `step` closure receives `shell`,
/// the plugin name, the handler value, and the current accumulator,
/// and returns the next accumulator.  The plugin capabilities are pushed
/// around each call, so the closure need not touch the capabilities stack.
pub(super) fn fold_hook<T>(
    shell: &mut Shell,
    hook_name: &str,
    init: T,
    mut step: impl FnMut(&mut Shell, &str, &Value, T) -> T,
) -> T {
    let handlers: Vec<(String, Value, Capabilities)> = shell
        .registry
        .plugins
        .iter()
        .filter_map(|p| {
            p.hooks
                .get(hook_name)
                .map(|h| (p.name.clone(), h.clone(), p.capabilities.clone()))
        })
        .collect();

    let mut acc = init;
    for (name, handler, capabilities) in handlers {
        acc = shell.with_capabilities(capabilities, |shell| step(shell, &name, &handler, acc));
    }
    acc
}

/// Run a named lifecycle hook on all plugins, passing `args` to each handler.
pub(super) fn run_lifecycle_hook(shell: &mut Shell, hook_name: &str, args: &[Value]) {
    fold_hook(shell, hook_name, (), |shell, name, handler, ()| {
        if let Err(EvalSignal::Error(e)) = ral_core::evaluator::call_value_pub(handler, args, shell) {
            plugin_error(name, &format!("hook '{hook_name}' failed"), &e);
        }
    });
}

/// Snapshot rustyline history (most-recent-first) into the runtime so plugin
/// hooks can read it via `_editor 'history'`.
pub(super) fn snapshot_history(
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
    lock(runtime).history_entries = entries;
}
