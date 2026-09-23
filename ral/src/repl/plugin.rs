//! Plugin runtime: the host-side state threaded between the REPL loop,
//! rustyline callbacks, and plugin hook dispatches.
//!
//! The `Arc<Mutex<PluginRuntime>>` lives across rustyline's `Hinter` and
//! `Highlighter` (which require `Send + Sync`) and the REPL's own
//! keybinding dispatch.  It holds the canonical plugin list (`plugins`)
//! and the `keybindings_dirty` reconciliation flag directly, and
//! partitions the rest into three named substructs so each call site
//! reaches for only the slice it owns:
//!
//! - [`EditorHooks`] — prev-buffer change-detection state, the most recent
//!   hook outputs (ghost text, highlights), the keymap, and the history
//!   snapshot.  Touched by Hinter/Highlighter callbacks.
//! - [`Keybindings`] — pending keybinding flagged by rustyline's event
//!   handler, the buffer stack populated by `_ed-push`, and the key
//!   sequences currently bound with rustyline.
//! - [`DeferredDiagnostics`] — plugin error/warning messages buffered
//!   during readline for later flushing past line-erase escapes.
//!
//! One invariant is load-bearing: the lock is never held across a dispatch,
//! since the host answers the dispatch's enquiries from this very state.

pub(super) mod ed_builtins;
pub(super) mod editor;
pub(super) mod load;
pub(super) mod manifest;
pub(super) mod router;
pub(super) mod rustyline;

pub(super) use self::router::{KeyChord, KeyName, KeyRouter, Resolution};
// Production code reaches `parse_key_notation` straight from `router`
// (manifest.rs); this re-export exists only so keybinding.rs's test
// module can name it via `crate::repl::plugin`.
#[cfg(test)]
pub(super) use self::router::parse_key_notation;

use ral_core::HookName;
use ral_core::protocol::Transport;
use ral_core::serial::FOValue;
use ral_core::serial::datum::Datum;
use ral_core::sync::LockExt as _;
use std::time::Duration;

use self::editor::{EditorState, HighlightSpan, PluginContext};
use self::manifest::{LoadedPlugin, Manifest};
use super::enquiry::PluginNote;
use super::errfmt::{format_plugin_disabled, plugin_warning};
use super::frontend::EditBuffer;
use super::host::ReplHost;
use ral_core::text::byte_to_char;
// Anchored to the crate root: a bare `rustyline::` path here would resolve
// to the sibling `rustyline` module instead of the crate of the same name.
use ::rustyline::KeyEvent;
use ::rustyline::config::EditMode;
use std::sync::{Arc, Mutex, MutexGuard};

// ── Lock helper ─────────────────────────────────────────────────────────

pub(super) fn lock(m: &Arc<Mutex<PluginRuntime>>) -> MutexGuard<'_, PluginRuntime> {
    m.lock_ignore_poison()
}

/// A plugin load or unload failure.  The message carries no surface tag: the
/// display site that reports it (`cmd_error`, the ralrc loader) owns the
/// prefix, so it appears exactly once.  Shared by the loader, the unloader,
/// and the manifest parser.
pub(super) fn load_err(msg: impl std::fmt::Display) -> ral_core::types::Error {
    ral_core::types::Error::new(msg.to_string(), 1)
}

/// Which keymap the editor is in — the frontend-neutral reduction of
/// rustyline's `EditMode`, and the rc `edit_mode:` key's value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Keymap {
    #[default]
    Emacs,
    Vi,
}

impl From<Keymap> for EditMode {
    fn from(keymap: Keymap) -> Self {
        match keymap {
            Keymap::Vi => Self::Vi,
            Keymap::Emacs => Self::Emacs,
        }
    }
}

impl Datum for Keymap {
    fn encode(self) -> FOValue {
        match self {
            Self::Emacs => "emacs",
            Self::Vi => "vi",
        }
        .to_string()
        .encode()
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match v.as_str() {
            Some("emacs") => Ok(Self::Emacs),
            Some("vi") => Ok(Self::Vi),
            _ => Err(format!("expected \"emacs\" or \"vi\", got {}", v.shape())),
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

// ── Substructs ──────────────────────────────────────────────────────────

/// State touched by the editor's hook callbacks (`Hinter`, `Highlighter`,
/// buffer-change driver).  `previous` tracks the last (text, cursor) so
/// `run_buffer_change_hooks` can short-circuit no-op events.
#[derive(Default)]
pub(super) struct EditorHooks {
    previous: EditBuffer,
    /// Latest ghost text produced by a buffer-change hook.
    pub(super) ghost: Option<String>,
    /// Latest highlight spans, composited across all plugins.
    pub(super) highlights: Vec<HighlightSpan>,
    keymap: Keymap,
    /// History snapshot for `_ed-history`, most recent first.
    pub(super) history: Vec<String>,
}

/// Keybinding-side state.  `pending` is the keybinding rustyline flagged
/// inside its event handler; the REPL drains it after readline returns.
/// `buffers` is the stack populated by `_ed-push`.  `bound` records the
/// key sequences currently registered with rustyline so
/// [`rustyline::sync_plugins`]
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
/// `plugins` is the canonical list, told by the engine's load doors.
/// `keybindings_dirty` signals the next readline iteration that rustyline
/// should re-register key handlers.
#[derive(Default)]
pub(crate) struct PluginRuntime {
    pub(super) plugins: Vec<LoadedPlugin>,
    pub(super) keybindings_dirty: bool,
    /// The keybinding dispatch table, derived from `plugins`; rebuilt by
    /// [`Self::keybindings_changed`] whenever the list changes.
    pub(super) router: KeyRouter,
    pub(super) hooks: EditorHooks,
    pub(super) keybindings: Keybindings,
    pub(super) diagnostics: DeferredDiagnostics,
}

// ── Deferred diagnostics ────────────────────────────────────────────────

/// Buffer an already-rendered plugin diagnostic for the REPL loop to flush
/// after readline returns.
pub(super) fn defer_plugin_message(runtime: &Arc<Mutex<PluginRuntime>>, message: String) {
    lock(runtime).diagnostics.messages.push(message);
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
/// moment an unload compacts the list.  `binding_idx` indexes into that
/// one plugin's immutable keybinding list, so it stays valid for as long
/// as the named plugin is loaded.
pub(super) struct PendingKeybinding {
    pub(super) plugin: String,
    pub(super) binding_idx: usize,
    /// Cursor position as a byte offset into the line at the moment the key fired.
    pub(super) cursor_byte: usize,
}

// ── Circuit breaker ─────────────────────────────────────────────────────

/// Per-session health of one plugin hook, the circuit-breaker's state.
///
/// A buffer-change hook fires on every keystroke, so a slow or always-faulting
/// one must not run unbraked. Consecutive faults accumulate; a fault run that
/// reaches [`BUFFER_CHANGE_FAULT_LIMIT`], or any run that overruns
/// [`BUFFER_CHANGE_BUDGET`], trips the breaker — the hook is skipped for the
/// rest of the session. A success resets the fault count.
#[derive(Debug, Clone, Default)]
pub(crate) struct HookHealth {
    consecutive_faults: u32,
    disabled: bool,
}

impl HookHealth {
    pub(super) fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Fold one hook outcome into the health state, returning `true` exactly on
    /// the call that trips the breaker (so the caller emits the single disable
    /// diagnostic once). A timeout trips immediately; otherwise faults
    /// accumulate to `fault_limit` and a success clears the count.
    pub(super) fn record_outcome(&mut self, ok: bool, timed_out: bool, fault_limit: u32) -> bool {
        if self.disabled {
            return false;
        }
        if timed_out {
            self.disabled = true;
            return true;
        }
        if ok {
            self.consecutive_faults = 0;
            false
        } else {
            self.consecutive_faults += 1;
            if self.consecutive_faults >= fault_limit {
                self.disabled = true;
                true
            } else {
                false
            }
        }
    }
}

/// Foreground wall for a single buffer-change hook run: a hook that runs this
/// long on one keystroke has overrun its keystroke budget and trips the
/// breaker. The wall is cooperative — the machine polls cancellation at
/// every step, so any handler doing ordinary work (iteration, command
/// spawns, recursion) is preempted at the next step.
const BUFFER_CHANGE_BUDGET: Duration = Duration::from_millis(100);

/// Consecutive buffer-change faults that trip the breaker.
const BUFFER_CHANGE_FAULT_LIMIT: u32 = 3;

// ── Buffer-change hooks ─────────────────────────────────────────────────

/// Drive buffer-change hooks whenever the line or cursor moves.
/// Called from `Hinter::hint()`, which holds no lock; the runtime lock is
/// taken and released around each dispatch.
///
/// `pos` is the byte offset rustyline supplies; it is converted once to a
/// character offset so everything downstream (the change-detection snapshot,
/// the editor state exposed to plugins, the hook's argument) speaks the same
/// units as the rest of the `_ed-*` surface.
pub(super) fn run_buffer_change_hooks(
    t: &dyn Transport,
    host: &Arc<ReplHost>,
    line: &str,
    pos: usize,
) {
    let runtime = &host.runtime;
    let pos = byte_to_char(line, pos);
    let (old_buf, handlers, history, keymap) = {
        let mut rt = lock(runtime);
        if line == rt.hooks.previous.text && pos == rt.hooks.previous.cursor {
            return;
        }
        let old_buf = std::mem::replace(&mut rt.hooks.previous.text, line.to_string());
        rt.hooks.previous.cursor = pos;

        let handlers = rt.with_hook("buffer-change", |p| !p.buffer_change_health.is_disabled());
        if handlers.is_empty() {
            rt.hooks.ghost = None;
            rt.hooks.highlights.clear();
            return;
        }
        (
            old_buf,
            handlers,
            rt.hooks.history.clone(),
            keymap_name(rt.hooks.keymap),
        )
    };

    let history_list = history.clone().encode();
    let mut ghost: Option<String> = None;
    let mut spans: Vec<HighlightSpan> = Vec::new();
    for name in handlers {
        let state_cell = lock(runtime).state_cell(&name);
        let arg = FOValue::Map {
            entries: vec![
                ("old_buf".into(), old_buf.clone().encode()),
                ("line".into(), line.to_string().encode()),
                (
                    "pos".into(),
                    FOValue::Int {
                        value: pos.try_into().unwrap_or(i64::MAX),
                    },
                ),
                ("history".into(), history_list.clone()),
                ("keymap".into(), keymap.to_string().encode()),
                ("state".into(), state_cell.clone().unwrap_or(FOValue::Unit)),
            ],
        };
        let ctx = PluginContext {
            editor_state: EditorState {
                text: line.to_string(),
                cursor: pos,
                keymap: keymap.into(),
            },
            history: history.clone(),
            in_readline: true,
            state_cell,
            ..PluginContext::default()
        };
        // The keystroke budget arms the wall, and the breaker disables a
        // persistently bad hook for the session.
        let hr = host.run_hook(
            t,
            HookName::plugin(name.clone(), "buffer-change"),
            vec![arg],
            Some(BUFFER_CHANGE_BUDGET),
            Some(ctx),
        );

        let state_cell = hr.ctx.map(|ctx| {
            if let Some(g) = ctx.outputs.ghost_text {
                ghost = Some(g);
            }
            spans.extend(ctx.outputs.highlight_spans);
            ctx.state_cell
        });
        let mut rt = lock(runtime);
        if let Some(state_cell) = state_cell {
            rt.write_back_state_cell(&name, state_cell);
        }
        if let Some(fault) = &hr.fault {
            rt.diagnostics.messages.push(fault.clone());
        }
        if let Some(p) = rt.plugins.iter_mut().find(|p| p.name == name)
            && p.buffer_change_health.record_outcome(
                hr.fault.is_none(),
                hr.walled,
                BUFFER_CHANGE_FAULT_LIMIT,
            )
        {
            let why = if hr.walled {
                format!(
                    "overran its {}ms keystroke budget",
                    BUFFER_CHANGE_BUDGET.as_millis()
                )
            } else {
                format!("failed {BUFFER_CHANGE_FAULT_LIMIT} times in a row")
            };
            let msg = format_plugin_disabled(&name, "buffer-change", &why);
            rt.diagnostics.messages.push(msg);
        }
    }

    let mut rt = lock(runtime);
    rt.hooks.ghost = ghost;
    rt.hooks.highlights = spans;
}

// ── Plugin lifecycle helpers ─────────────────────────────────────────────

/// Reset the per-read editor state before a read begins.
pub(super) fn reset_editor_hooks(runtime: &Arc<Mutex<PluginRuntime>>, keymap: Keymap) {
    let mut rt = lock(runtime);
    rt.hooks.keymap = keymap;
    rt.hooks.previous = EditBuffer::default();
    rt.hooks.ghost = None;
    rt.hooks.highlights.clear();
}

impl PluginRuntime {
    /// The plugins registering `event` that pass `keep`, by name.
    pub(super) fn with_hook(
        &self,
        event: &str,
        keep: impl Fn(&LoadedPlugin) -> bool,
    ) -> Vec<String> {
        self.plugins
            .iter()
            .filter(|p| p.hooks.iter().any(|h| h == event) && keep(p))
            .map(|p| p.name.clone())
            .collect()
    }

    /// Resolve a pending keybinding to its bound key notation.
    ///
    /// Lookup is by the plugin's name, the only identity stable across an
    /// unload's `Vec::remove`; `binding_idx` then indexes that one plugin's
    /// immutable keybinding list.  Returns `None` when the named plugin is
    /// no longer loaded — the unbind-and-ignore case.
    pub(super) fn resolve_keybinding(&self, plugin: &str, binding_idx: usize) -> Option<String> {
        let p = self.plugins.iter().find(|p| p.name == plugin)?;
        let kb = p.keybindings.get(binding_idx)?;
        Some(kb.key.clone())
    }

    /// Fetch a plugin's persistent state cell by name.
    pub(super) fn state_cell(&self, name: &str) -> Option<FOValue> {
        self.plugins
            .iter()
            .find(|p| p.name == name)
            .and_then(|p| p.state_cell.clone())
    }

    /// Record that the plugin list changed: rebuild the dispatch table and
    /// flag rustyline for re-registration on the next readline iteration.
    pub(super) fn keybindings_changed(&mut self) {
        self.router = KeyRouter::build(&self.plugins);
        self.keybindings_dirty = true;
    }

    /// Save a handler's (possibly mutated) state cell back into the named
    /// plugin's record.  A no-op if the plugin was unloaded mid-dispatch.
    pub(super) fn write_back_state_cell(&mut self, name: &str, state_cell: Option<FOValue>) {
        if let Some(p) = self.plugins.iter_mut().find(|p| p.name == name) {
            p.state_cell = state_cell;
        }
    }

    /// Take what a load door says: admit a manifest, re-validated, or let a
    /// plugin go.  The engine's own admission checks do not excuse these.
    pub(super) fn note(&mut self, note: PluginNote) -> Result<(), String> {
        match note {
            PluginNote::Loaded(manifest) => self.admit(manifest),
            PluginNote::Unloaded(name) => {
                let idx = self
                    .plugins
                    .iter()
                    .position(|p| p.name == name)
                    .ok_or_else(|| format!("plugin '{name}' is not loaded"))?;
                self.plugins.remove(idx);
                self.keybindings_changed();
                Ok(())
            }
        }
    }

    fn admit(&mut self, manifest: Manifest) -> Result<(), String> {
        if self.plugins.iter().any(|p| p.name == manifest.name) {
            return Err(format!("plugin '{}' is already loaded", manifest.name));
        }
        let plugin = LoadedPlugin::admit(manifest)?;
        let name = plugin.name.clone();
        self.plugins.push(plugin);
        self.keybindings_changed();
        // Shadow lint: a dead binding (an earlier unguarded entry owns its
        // chord) can only be introduced by this load, and only among this
        // plugin's own entries — earlier plugins keep their precedence.
        for (dead, blocker) in self.router.dead_entries() {
            if dead.plugin == name {
                plugin_warning(
                    &name,
                    &format!(
                        "keybinding '{}' will never fire: an unguarded '{}' binding of plugin \
                         '{}' precedes it",
                        dead.key, blocker.key, blocker.plugin
                    ),
                );
            }
        }
        Ok(())
    }
}

/// Fire lifecycle `event` on every plugin that registers it.  Every
/// lifecycle hook receives exactly one argument, the event record: `{src}`
/// for `pre-exec`, `{src, status}` for `post-exec`, `{old, new}` for `chpwd`.
pub(crate) fn fire(t: &dyn Transport, host: &Arc<ReplHost>, event: &str, record: &FOValue) {
    let names = lock(&host.runtime).with_hook(event, |_| true);
    for name in names {
        let hr = host.run_hook(
            t,
            HookName::plugin(name, event),
            vec![record.clone()],
            None,
            None,
        );
        // Printed, not deferred: a lifecycle hook runs with the terminal
        // already stable, and a deferred message would wait for a next prompt
        // the last command before exit never reaches.
        if let Some(fault) = hr.fault {
            eprintln!("{fault}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Keymap` names rustyline's `EditMode`, the rc's `edit_mode:` value, and
    /// the keymap for the `_ed-keymap` query.
    #[test]
    fn keymap_names_edit_mode_rc_value_and_plugin_keymap() {
        assert_eq!(EditMode::from(Keymap::Vi), EditMode::Vi);
        assert_eq!(Keymap::decode(&Keymap::Vi.encode()), Ok(Keymap::Vi));
        assert_eq!(keymap_name(Keymap::Vi), "viins");
        assert_eq!(keymap_name(Keymap::Emacs), "emacs");
    }
}
