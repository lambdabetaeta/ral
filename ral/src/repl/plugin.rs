//! Plugin runtime: shared mutable state threaded between the REPL loop,
//! rustyline callbacks, and plugin handler invocations.
//!
//! The `Arc<Mutex<PluginRuntime>>` lives across rustyline's `Hinter` and
//! `Highlighter` (which require `Send + Sync`) and the REPL's own
//! keybinding dispatch.  The runtime is partitioned into four named
//! substructs so each call site reaches for only the slice it owns:
//!
//! - [`PluginSnapshot`] — the plugin list and the generation it was
//!   synced from.  Read by every hook path; mutated by [`sync_plugins`].
//! - [`EditorHooks`] — hook env, prev-buffer change-detection state,
//!   the most recent hook outputs (ghost text, highlights), the
//!   editor state exposed to plugins via `_ed-*`, and the history
//!   snapshot.  Touched by Hinter/Highlighter callbacks.
//! - [`Keybindings`] — pending keybinding flagged by rustyline's event
//!   handler and the buffer stack populated by `_ed-push`.
//! - [`DeferredDiagnostics`] — plugin error/warning messages buffered
//!   during readline for later flushing past line-erase escapes.
//!
//! One invariant is load-bearing: editor callbacks may touch this
//! mutex, but the evaluator must not.  Every hook call releases the
//! lock before invoking ral code so re-entrant `_ed-*` builtins can
//! re-acquire it.

pub(super) mod load;
pub(super) mod manifest;

use ral_core::types::{Break, Settled};
use ral_core::{Shell, Value};

use self::manifest::LoadedPlugin;
use super::errfmt::{format_plugin_error, plugin_error, plugin_warning};
use super::frontend::EditBuffer;
use super::plugin_editor::{
    EditorState, HighlightSpan, PluginContext, PluginInputs, PluginOutputs, byte_to_char,
};
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

/// Which keymap the editor is in — the frontend-neutral reduction of
/// rustyline's `EditMode`.  Both editor backends carry one of these to the
/// shared hook/keybinding primitives so neither has to thread rustyline's
/// type through code that only needs to name the keymap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Keymap {
    Emacs,
    Vi,
}

impl From<EditMode> for Keymap {
    fn from(mode: EditMode) -> Self {
        match mode {
            EditMode::Vi => Keymap::Vi,
            _ => Keymap::Emacs,
        }
    }
}

/// Name the keymap for plugin hooks: `"viins"` for vi insert, `"emacs"`
/// otherwise.  Surfaced to plugin hooks via the `_ed-keymap` query.
pub(super) fn keymap_name(keymap: Keymap) -> &'static str {
    match keymap {
        Keymap::Vi => "viins",
        Keymap::Emacs => "emacs",
    }
}

/// A frontend-neutral key name: the subset of keys a plugin keybinding may
/// bind, exactly the notations [`parse_key_notation`] accepts.  Each editor
/// backend adapts these to its own event type at its boundary — rustyline's
/// `KeyEvent` ([`chord_to_key_event`]), crossterm's in the structural surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum KeyName {
    Char(char),
    Tab,
    Enter,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    Backspace,
    F(u8),
}

/// A frontend-neutral key chord: a [`KeyName`] plus the ctrl/alt modifiers.
/// [`parse_key_notation`] produces these; the backends compare/adapt them
/// without ever seeing each other's event types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct KeyChord {
    pub(super) name: KeyName,
    pub(super) ctrl: bool,
    pub(super) alt: bool,
}

// ── Substructs ──────────────────────────────────────────────────────────

/// State touched by the editor's hook callbacks (`Hinter`, `Highlighter`,
/// buffer-change driver).  `previous` tracks the last (text, cursor) so
/// `run_buffer_change_hooks` can short-circuit no-op events.
#[derive(Default)]
pub(super) struct EditorHooks {
    /// Shell snapshot used to evaluate hooks inside rustyline callbacks.
    /// `None` outside a readline session.
    pub(super) env: Option<Shell>,
    /// Previous (text, cursor) at the last buffer-change check — used
    /// purely for change detection, distinct from `state` below.
    previous: EditBuffer,
    /// Latest ghost text produced by a buffer-change hook.
    pub(super) ghost: Option<String>,
    /// Latest highlight spans, composited across all plugins.
    pub(super) highlights: Vec<HighlightSpan>,
    /// Editor state exposed to plugin hooks via the `_ed-*` builtins.
    pub(super) state: EditorState,
    /// History snapshot for `_ed-history`.
    pub(super) history: Vec<String>,
}

/// Keybinding-side state.  `pending` is the keybinding rustyline flagged
/// inside its event handler; the REPL drains it after readline returns.
/// `buffers` is the stack populated by `_ed-push`.  `bound` records the
/// key sequences currently registered with rustyline so [`sync_plugins`]
/// can unbind the ones an unloaded plugin owned — rustyline keys nothing
/// on plugin identity, so we hold the reconciliation set here.
#[derive(Default)]
pub(super) struct Keybindings {
    pub(super) pending: Option<PendingKeybinding>,
    pub(super) buffers: Vec<EditBuffer>,
    pub(super) bound: Vec<KeyEvent>,
}

/// Plugin diagnostics buffered during a readline session so the REPL can
/// flush them past the `\x1b[A\r\x1b[K` line-erase that follows
/// `Cmd::AcceptLine` — printing them immediately would land on a line
/// that the escape clobbers.
#[derive(Default)]
pub(super) struct DeferredDiagnostics {
    pub(super) messages: Vec<String>,
}

/// Aggregated plugin runtime.  Wrapped in `Arc<Mutex<>>` because
/// rustyline requires `ConditionalEventHandler: Send + Sync`; the
/// substructs themselves are owned by value here.
///
/// `plugins` is the canonical list — owned directly, not synced from
/// any other store.  `keybindings_dirty` is set by `load` / `unload`
/// to signal the next readline iteration that rustyline should
/// re-register key handlers.
#[derive(Default)]
pub(crate) struct PluginRuntime {
    pub(super) plugins: Vec<LoadedPlugin>,
    pub(super) keybindings_dirty: bool,
    pub(super) hooks: EditorHooks,
    pub(super) keybindings: Keybindings,
    pub(super) diagnostics: DeferredDiagnostics,
}

// ── Deferred diagnostics ────────────────────────────────────────────────

/// Buffer a plugin error for the REPL loop to flush after readline returns.
///
/// Use this from inside the readline loop (keybinding dispatch,
/// buffer-change hooks).  For one-shot lifecycle paths where no escape
/// sequence is pending, [`errfmt::plugin_error`](super::errfmt::plugin_error)
/// may be called directly.
pub(super) fn defer_plugin_error(
    runtime: &Arc<Mutex<PluginRuntime>>,
    plugin_name: &str,
    context: &str,
    err: &ral_core::types::Error,
) {
    lock(runtime)
        .diagnostics
        .messages
        .push(format_plugin_error(plugin_name, context, err));
}

/// Drain and write any buffered plugin diagnostics to stderr.
///
/// Called by the editor at points where the terminal is in a stable
/// state (after any line-erase escape, before the next prompt) so each
/// message lands on its own durable line above the prompt.
pub(crate) fn flush_pending_messages(runtime: &Arc<Mutex<PluginRuntime>>) {
    let msgs: Vec<String> = std::mem::take(&mut lock(runtime).diagnostics.messages);
    for m in msgs {
        eprintln!("{m}");
    }
}

/// Drain one entry from the plugin buffer stack (`_ed-push`).  Both editor
/// backends pop a pushed buffer when the session hands them no pending one,
/// so the pop lives here rather than being duplicated per frontend.
pub(super) fn pop_buffer_stack(runtime: &Arc<Mutex<PluginRuntime>>) -> Option<EditBuffer> {
    lock(runtime).keybindings.buffers.pop()
}

/// A keybinding flagged by rustyline's event handler, identified by the
/// owning plugin's name (unique and stable across loads/unloads) rather
/// than its position in the runtime `Vec` — an index goes stale the
/// moment `unload_plugin` compacts the list.  `binding_idx` indexes into
/// that one plugin's immutable keybinding list, so it stays valid for as
/// long as the named plugin is loaded.
pub(super) struct PendingKeybinding {
    pub(super) plugin: String,
    pub(super) binding_idx: usize,
    /// Cursor position as a byte offset into the line at the moment the key fired.
    pub(super) cursor_byte: usize,
}

// ── Transactional hook helper ───────────────────────────────────────────

/// Per-call view of a plugin's metadata threaded through the hook
/// helper.  `name` is used for diagnostic context.  Plugins run with
/// host authority; no capability attenuation is applied around the
/// hook call.
pub(super) struct HookFor<'a> {
    pub(super) name: &'a str,
}

/// Outcome of one [`call_plugin_hook`] invocation.
///
/// `ctx` is `Some` iff the caller passed `Some` for `ctx_in`; the caller
/// inspects `outputs` (ghost text, highlights, pushed buffer, accept
/// flag) and `state_cell` (to save back into the plugin record).
pub(super) struct HookResult {
    pub(super) result: Settled<Value>,
    pub(super) ctx: Option<PluginContext>,
}

/// The single primitive for invoking a plugin hook.  In order:
///   1. take any pre-existing `shell.repl().plugin_context` aside
///   2. install `ctx_in` (when `Some`) so `_ed-*` builtins resolve correctly
///   3. call the handler with `args` (no capability attenuation; plugins run with host authority)
///   4. take the context back out (now carrying outputs and any state_cell mutation)
///   5. restore the pre-existing context
///
/// State cell flows through `ctx_in.state_cell` / `result.ctx.state_cell`
/// — the helper does not touch the plugin record itself, so it works
/// uniformly whether the plugin lives in the live shell registry (for
/// lifecycle/prompt hooks) or behind the runtime mutex (for buffer-change
/// / keybinding hooks).
pub(super) fn call_plugin_hook(
    shell: &mut Shell,
    plugin: HookFor<'_>,
    hook: &Value,
    args: &[Value],
    ctx_in: Option<PluginContext>,
) -> HookResult {
    let _ = plugin.name; // reserved for future error-context use
    let prev = shell.repl_mut().plugin_context.take();
    if let Some(ctx) = ctx_in {
        shell.repl_mut().plugin_context = Some(Box::new(ctx));
    }
    let result = ral_core::evaluator::apply(hook.clone(), args.to_vec(), shell);
    let ctx = shell
        .repl_mut()
        .plugin_context
        .take()
        .and_then(|b| b.downcast::<PluginContext>().ok().map(|b| *b));
    shell.repl_mut().plugin_context = prev;
    HookResult { result, ctx }
}

// ── Buffer-change hooks ─────────────────────────────────────────────────

/// Drive buffer-change hooks whenever the line or cursor moves.
/// Called from `Hinter::hint()`, which holds no lock; we acquire and release
/// the runtime lock around each evaluator call to avoid re-entrancy.
///
/// `pos` is the byte offset rustyline supplies; it is converted once to a
/// character offset so everything downstream (the change-detection snapshot,
/// the `EditorState` exposed to plugins, the hook's third argument) speaks
/// the same units as the rest of the `_ed-*` surface.
pub(super) fn run_buffer_change_hooks(runtime: &Arc<Mutex<PluginRuntime>>, line: &str, pos: usize) {
    let pos = byte_to_char(line, pos);
    // ── Phase 1: collect work items under lock, then release ─────────────
    let (old_buf, handlers, mut hook_env, history, keymap) = {
        let mut rt = lock(runtime);
        if line == rt.hooks.previous.text && pos == rt.hooks.previous.cursor {
            return;
        }
        let old_buf = std::mem::replace(&mut rt.hooks.previous.text, line.to_string());
        rt.hooks.previous.cursor = pos;

        let handlers: Vec<(usize, String, Value)> = rt
            .plugins
            .iter()
            .enumerate()
            .filter_map(|(i, p)| {
                p.hooks
                    .get("buffer-change")
                    .map(|h| (i, p.name.clone(), h.clone()))
            })
            .collect();

        if handlers.is_empty() {
            rt.hooks.ghost = None;
            rt.hooks.highlights.clear();
            return;
        }

        let Some(hook_env) = rt.hooks.env.take() else {
            return;
        };
        let history = rt.hooks.history.clone();
        let keymap = rt.hooks.state.keymap.clone();
        (old_buf, handlers, hook_env, history, keymap)
    }; // lock released

    let args = [
        Value::String(old_buf),
        Value::String(line.to_string()),
        Value::Int(pos as i64),
    ];
    let mut ghost: Option<String> = None;
    let mut spans: Vec<HighlightSpan> = Vec::new();

    // ── Phase 2: run each handler with lock released around evaluator ────
    for (idx, name, handler) in handlers {
        // Snapshot state cell from the runtime; the helper will load it into
        // the PluginContext and surface any mutations back through `ctx_out`.
        let state_cell = lock(runtime)
            .plugins
            .get(idx)
            .and_then(|p| p.state_cell.clone());
        let state_loaded = state_cell.is_some();

        let ctx_in = PluginContext {
            editor_state: EditorState {
                text: line.to_string(),
                cursor: pos,
                keymap: keymap.clone(),
            },
            inputs: PluginInputs {
                history_entries: history.clone(),
                in_readline: true,
            },
            outputs: PluginOutputs::default(),
            state_cell,
            state_default_used: state_loaded,
        };

        let hr = call_plugin_hook(
            &mut hook_env,
            HookFor { name: &name },
            &handler,
            &args,
            Some(ctx_in),
        );

        if let Err(Break::Error(e)) = &hr.result {
            defer_plugin_error(runtime, &name, "hook 'buffer-change' failed", e);
        }

        if let Some(ctx_out) = hr.ctx {
            // Save state cell back to the runtime.
            {
                let mut rt = lock(runtime);
                if let Some(p) = rt.plugins.get_mut(idx) {
                    p.state_cell = ctx_out.state_cell.clone();
                }
            }
            if let Some(g) = ctx_out.outputs.ghost_text {
                ghost = Some(g);
            }
            spans.extend(ctx_out.outputs.highlight_spans);
        }
    }

    // ── Phase 3: store results, return hook_env to runtime ───────────────
    let mut rt = lock(runtime);
    rt.hooks.ghost = ghost;
    rt.hooks.highlights = spans;
    rt.hooks.env = Some(hook_env);
}

// ── Keybinding handlers ─────────────────────────────────────────────────

pub(super) struct PluginKeyHandler {
    pub(super) runtime: Arc<Mutex<PluginRuntime>>,
    pub(super) plugin: String,
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
            rt.keybindings.pending = Some(PendingKeybinding {
                plugin: self.plugin.clone(),
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
/// frontend-neutral [`KeyChord`].  Returns `None` for unrecognised notations.
pub(super) fn parse_key_notation(key: &str) -> Option<KeyChord> {
    const NAMED: &[(&str, KeyName)] = &[
        ("tab", KeyName::Tab),
        ("enter", KeyName::Enter),
        ("escape", KeyName::Escape),
        ("up", KeyName::Up),
        ("down", KeyName::Down),
        ("left", KeyName::Left),
        ("right", KeyName::Right),
        ("home", KeyName::Home),
        ("end", KeyName::End),
        ("delete", KeyName::Delete),
        ("backspace", KeyName::Backspace),
    ];
    let plain = |name| KeyChord {
        name,
        ctrl: false,
        alt: false,
    };
    let key = key.trim();
    if key.len() == 1 {
        return Some(plain(KeyName::Char(key.chars().next()?)));
    }
    if let Some(&(_, name)) = NAMED.iter().find(|(n, _)| *n == key) {
        return Some(plain(name));
    }
    for (prefix, ctrl, alt) in [("ctrl-", true, false), ("alt-", false, true)] {
        if let Some(rest) = key.strip_prefix(prefix) {
            return Some(KeyChord {
                name: KeyName::Char(rest.chars().next()?),
                ctrl,
                alt,
            });
        }
    }
    let num = key.strip_prefix('f').and_then(|s| s.parse::<u8>().ok())?;
    (1..=12).contains(&num).then_some(plain(KeyName::F(num)))
}

/// Adapt a [`KeyChord`] to the rustyline `KeyEvent` rustyline binds against.
/// The rustyline boundary — [`sync_plugins`] is the only caller.
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

// ── Plugin lifecycle helpers ─────────────────────────────────────────────

/// Reconcile plugin keybindings with rustyline when `load` / `unload`
/// has touched the plugin list.  The `keybindings_dirty` flag is set by
/// those routines and cleared here; if it isn't set we skip the work
/// (no plugins changed since last sync).
///
/// rustyline owns the binding table but keys nothing on plugin identity,
/// so reconciliation is a full unbind-then-rebind: every sequence bound
/// on the last sync is dropped, then the live plugin list is re-walked.
/// A plugin removed by `unload` therefore loses its sequences here rather
/// than leaving a handler whose name now resolves to nothing.
pub(super) fn sync_plugins(
    runtime: &Arc<Mutex<PluginRuntime>>,
    rl: &mut Editor<RalHelper, DefaultHistory>,
) {
    let plugins: Vec<(String, Vec<String>)> = {
        let mut rt = lock(runtime);
        if !rt.keybindings_dirty {
            return;
        }
        rt.keybindings_dirty = false;
        for key in std::mem::take(&mut rt.keybindings.bound) {
            rl.unbind_sequence(key);
        }
        rt.plugins
            .iter()
            .map(|p| {
                (
                    p.name.clone(),
                    p.keybindings.iter().map(|(k, _)| k.clone()).collect(),
                )
            })
            .collect()
    };

    let mut bound = Vec::new();
    for (name, keys) in &plugins {
        for (bi, key_str) in keys.iter().enumerate() {
            if let Some(key_event) = parse_key_notation(key_str).map(chord_to_key_event) {
                rl.bind_sequence(
                    key_event,
                    rustyline::EventHandler::Conditional(Box::new(PluginKeyHandler {
                        runtime: runtime.clone(),
                        plugin: name.clone(),
                        binding_idx: bi,
                    })),
                );
                bound.push(key_event);
            } else {
                plugin_warning(name, &format!("invalid key notation '{key_str}', skipping"));
            }
        }
    }
    lock(runtime).keybindings.bound = bound;
}

/// Prepare the hook shell and reset per-readline state before entering readline.
///
/// The hook shell carries no `PluginContext` of its own — each
/// [`call_plugin_hook`] installs one for the duration of the call and
/// takes it back afterwards.  This keeps the persistent fields
/// (`hooks.state`, `hooks.history`) as the source of truth and lets us
/// build a fresh context per handler from them.
pub(super) fn prepare_hook_env(
    shell: &Shell,
    runtime: &Arc<Mutex<PluginRuntime>>,
    keymap: Keymap,
) {
    let mut rt = lock(runtime);

    rt.hooks.state = EditorState {
        text: String::new(),
        cursor: 0,
        keymap: keymap_name(keymap).into(),
    };
    rt.hooks.previous = EditBuffer::default();
    rt.hooks.ghost = None;
    rt.hooks.highlights.clear();

    let mut hook_env = Shell::child_from(&shell.snapshot(), shell);
    hook_env.set_interactive(true);
    rt.hooks.env = Some(hook_env);
}

/// RAII guard that releases the hook shell when dropped.  Construct after
/// `prepare_hook_env`; dropping it on any exit path clears `hooks.env` so
/// subsequent buffer-change hooks bail cleanly until the next prepare.
///
/// Holds its own `Arc` clone (not a borrow) so the frontend can keep
/// using `&mut self` for readline calls without borrow-checker conflict.
pub(super) struct HookEnvGuard(pub(super) Arc<Mutex<PluginRuntime>>);

impl Drop for HookEnvGuard {
    fn drop(&mut self) {
        lock(&self.0).hook_env_clear();
    }
}

impl PluginRuntime {
    /// Release the hook shell.  Invoked via [`HookEnvGuard`] on every
    /// readline exit path.
    fn hook_env_clear(&mut self) {
        self.hooks.env = None;
    }

    /// Resolve a pending keybinding to its plugin index and handler value.
    ///
    /// Lookup is by the plugin's name, the only identity stable across
    /// `unload_plugin`'s `Vec::remove`; `binding_idx` then indexes that
    /// one plugin's immutable keybinding list.  Returns `None` when the
    /// named plugin is no longer loaded — the unbind-and-ignore case.
    pub(super) fn resolve_keybinding(
        &self,
        plugin: &str,
        binding_idx: usize,
    ) -> Option<(usize, Value)> {
        let idx = self.plugins.iter().position(|p| p.name == plugin)?;
        let (_, handler) = self.plugins[idx].keybindings.get(binding_idx)?;
        Some((idx, handler.clone()))
    }

    /// Every plugin keybinding as a frontend-neutral `(plugin_name,
    /// binding_idx, chord)` triple, for a frontend that matches incoming
    /// keys itself rather than registering handlers with rustyline (the
    /// structural surface).  The `(plugin_name, binding_idx)` pair is the
    /// same identity [`resolve_keybinding`] resolves, so a matched chord
    /// dispatches through the shared [`super::keybinding::dispatch_keybinding`].
    /// An unparseable notation is skipped silently — rustyline's
    /// [`sync_plugins`] already warns on it, and this runs every keypress.
    #[cfg(feature = "structural")]
    pub(super) fn keybinding_chords(&self) -> Vec<(String, usize, KeyChord)> {
        let mut out = Vec::new();
        for p in &self.plugins {
            for (bi, (key_str, _)) in p.keybindings.iter().enumerate() {
                if let Some(chord) = parse_key_notation(key_str) {
                    out.push((p.name.clone(), bi, chord));
                }
            }
        }
        out
    }
}

/// Fold a named hook over all plugins that register it, threading an
/// accumulator through each call.  The `step` closure receives `shell`,
/// a [`HookFor`] view of the plugin, the handler value, and the current
/// accumulator; it returns the next accumulator.  Typical bodies invoke
/// [`call_plugin_hook`] to install the PluginContext around the call —
/// `fold_hook` itself only collects handlers and threads `acc`.
pub(crate) fn fold_hook<T>(
    runtime: &Arc<Mutex<PluginRuntime>>,
    shell: &mut Shell,
    hook_name: &str,
    init: T,
    mut step: impl FnMut(&mut Shell, HookFor<'_>, &Value, T) -> T,
) -> T {
    let handlers: Vec<(String, Value)> = lock(runtime)
        .plugins
        .iter()
        .filter_map(|p| p.hooks.get(hook_name).map(|h| (p.name.clone(), h.clone())))
        .collect();

    let mut acc = init;
    for (name, handler) in handlers {
        acc = step(shell, HookFor { name: &name }, &handler, acc);
    }
    acc
}

/// Run a named lifecycle hook on all plugins, passing `args` to each handler.
pub(crate) fn run_lifecycle_hook(
    runtime: &Arc<Mutex<PluginRuntime>>,
    shell: &mut Shell,
    hook_name: &str,
    args: &[Value],
) {
    fold_hook(
        runtime,
        shell,
        hook_name,
        (),
        |shell, plugin, handler, ()| {
            let plugin_name = plugin.name.to_string();
            let hr = call_plugin_hook(shell, plugin, handler, args, None);
            if let Err(Break::Error(e)) = &hr.result {
                plugin_error(&plugin_name, &format!("hook '{hook_name}' failed"), e);
            }
        },
    );
}

/// Snapshot rustyline history (most-recent-first) into the runtime so plugin
/// hooks can read it via `_ed-history`.
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
    lock(runtime).hooks.history = entries;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `parse_key_notation` reduces every supported notation to a
    /// frontend-neutral [`KeyChord`] — the modifiers and the named keys the
    /// fzf/zoxide plugins bind, plus the bare-char and function-key forms.
    #[test]
    fn parse_key_notation_yields_neutral_chords() {
        let chord = |name, ctrl, alt| Some(KeyChord { name, ctrl, alt });
        assert_eq!(
            parse_key_notation("ctrl-r"),
            chord(KeyName::Char('r'), true, false)
        );
        assert_eq!(
            parse_key_notation("alt-c"),
            chord(KeyName::Char('c'), false, true)
        );
        assert_eq!(parse_key_notation("tab"), chord(KeyName::Tab, false, false));
        assert_eq!(
            parse_key_notation("t"),
            chord(KeyName::Char('t'), false, false)
        );
        assert_eq!(parse_key_notation("f5"), chord(KeyName::F(5), false, false));
        // Unrecognised notations and out-of-range function keys are rejected.
        assert_eq!(parse_key_notation("hyper-x"), None);
        assert_eq!(parse_key_notation("f13"), None);
    }

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

    /// `Keymap` is the neutral reduction of rustyline's `EditMode`, and names
    /// the keymap for the `_ed-keymap` query the same way the old code did.
    #[test]
    fn keymap_reduces_edit_mode_and_names_it() {
        assert_eq!(Keymap::from(EditMode::Vi), Keymap::Vi);
        assert_eq!(Keymap::from(EditMode::Emacs), Keymap::Emacs);
        assert_eq!(keymap_name(Keymap::Vi), "viins");
        assert_eq!(keymap_name(Keymap::Emacs), "emacs");
    }
}
