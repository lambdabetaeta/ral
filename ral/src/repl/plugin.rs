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
//!   handler and the buffer stack populated by `_ed-push`.
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

use ral_core::HookName;
use ral_core::carrier::Transport;
use ral_core::errln;
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum;
use ral_core::sync::LockExt as _;
use ral_core::ty::{Ty, Typed};
use std::time::Duration;

use self::editor::{EditorState, HighlightSpan, PluginContext};
use self::manifest::{HookEvent, LoadedPlugin, Manifest};
use super::completion::{Candidate, parse_at};
use super::enquiry::PluginNote;
use super::errfmt::{format_plugin_disabled, format_plugin_warning};
use super::frontend::EditBuffer;
use super::host::{HookResult, Outcome, ReplHost};
use ral_core::text::{byte_to_char, char_to_byte};
// Anchored to the crate root: a bare `rustyline::` path here would resolve
// to the sibling `rustyline` module instead of the crate of the same name.
use ::rustyline::config::EditMode;
use std::sync::{Arc, Mutex, MutexGuard};

// ── Lock helper ─────────────────────────────────────────────────────────

pub(super) fn lock(m: &Arc<Mutex<PluginRuntime>>) -> MutexGuard<'_, PluginRuntime> {
    m.lock_ignore_poison()
}

/// A plugin load or unload failure.  The message carries no surface tag: the
/// display site that reports it owns any prefix, so it appears at most once.
/// Shared by the loader, the unloader, and the manifest parser.
pub(super) fn load_err(msg: impl std::fmt::Display) -> ral_core::types::Error {
    ral_core::types::Error::new(msg.to_string())
}

/// Which keymap the editor is in — the frontend-neutral reduction of
/// rustyline's `EditMode`, the rc `edit-mode:` key's value, and `_ed-get`'s
/// `keymap`.
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

impl Typed for Keymap {
    fn ty() -> Ty {
        Ty::String
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
    pub(super) history: Arc<[String]>,
}

/// Keybinding-side state.  `pending` is the keybinding rustyline flagged
/// inside its event handler; the REPL drains it after readline returns.
/// `buffers` is the stack populated by `_ed-push`.
#[derive(Default)]
pub(super) struct Keybindings {
    pub(super) pending: Option<PendingKeybinding>,
    pub(super) buffers: Vec<EditBuffer>,
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
/// `plugins` is the canonical list, told by the engine's load builtins.
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
        errln!("{m}");
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
    /// The editor's input mode at the press: `insert`, `command` or `replace`.
    pub(super) mode: &'static str,
}

// ── Circuit breaker ─────────────────────────────────────────────────────

/// Per-session health of one plugin hook, the circuit-breaker's state.
///
/// A braked hook fires on every keystroke (`buffer-change`) or every Tab
/// (`complete`), so a slow or always-faulting one must not run unbraked.
/// Consecutive faults accumulate; a fault run that reaches [`FAULT_LIMIT`], or
/// any run that overruns its budget ([`BUFFER_CHANGE`], [`COMPLETE`]), trips
/// the breaker — the hook is skipped for the rest of the session. A run that
/// ends otherwise resets the fault count.
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
    /// diagnostic once). A wall trips immediately; otherwise faults accumulate
    /// to [`FAULT_LIMIT`] and any other ending, a stop included, clears the count.
    pub(super) fn record_outcome(&mut self, outcome: &Outcome) -> bool {
        if self.disabled {
            return false;
        }
        self.consecutive_faults = match outcome {
            Outcome::Faulted(_) => self.consecutive_faults + 1,
            _ => 0,
        };
        self.disabled =
            matches!(outcome, Outcome::Walled(_)) || self.consecutive_faults >= FAULT_LIMIT;
        self.disabled
    }
}

/// A braked hook: its event, and the wall each run must beat.
#[derive(Clone, Copy)]
struct Brake {
    event: HookEvent,
    budget: Duration,
}

/// Foreground wall for a single buffer-change hook run: a hook that runs this
/// long on one keystroke has overrun its keystroke budget and trips the
/// breaker. The wall is cooperative — the machine polls cancellation at
/// every step, so any handler doing ordinary work (iteration, command
/// spawns, recursion) is preempted at the next step.
const BUFFER_CHANGE: Brake = Brake {
    event: HookEvent::BufferChange,
    budget: Duration::from_millis(100),
};

/// The same wall, longer, for a `complete` hook, which runs on a Tab press and
/// may consult the disk or a command.
const COMPLETE: Brake = Brake {
    event: HookEvent::Complete,
    budget: Duration::from_millis(500),
};

/// Consecutive faults that trip the breaker.
const FAULT_LIMIT: u32 = 3;

/// Run `name`'s `brake` hook on `arg` against the buffer `line`, the cursor at
/// char `pos`: the plugin's state cell goes in and back out, a fault is
/// deferred, and the outcome is folded into the breaker, which defers its one
/// disable notice.
fn run_braked(
    t: &dyn Transport,
    host: &Arc<ReplHost>,
    name: &str,
    Brake { event, budget }: Brake,
    arg: FOValue,
    line: &str,
    pos: usize,
) -> HookResult {
    let (state_cell, history, keymap) = {
        let rt = lock(&host.runtime);
        (
            rt.state_cell(name),
            rt.hooks.history.clone(),
            rt.hooks.keymap,
        )
    };
    let ctx = PluginContext {
        editor_state: EditorState::new(line.to_owned(), pos, keymap),
        history,
        state_cell,
        ..PluginContext::default()
    };
    let mut hr = host.run_hook(
        t,
        HookName::plugin(name, <&str>::from(event)),
        vec![arg],
        Some(budget),
        Some(ctx),
    );
    let mut rt = lock(&host.runtime);
    if let Some(ctx) = &mut hr.ctx {
        rt.write_back_state_cell(name, ctx.state_cell.take());
    }
    if let Outcome::Faulted(fault) | Outcome::Walled(fault) | Outcome::Stopped(fault) = &hr.outcome
    {
        rt.diagnostics.messages.push(fault.clone());
    }
    if let Some(p) = rt.plugins.iter_mut().find(|p| p.name == name)
        && p.health
            .entry(event)
            .or_default()
            .record_outcome(&hr.outcome)
    {
        let why = if matches!(hr.outcome, Outcome::Walled(_)) {
            format!("overran its {}ms budget", budget.as_millis())
        } else {
            format!("failed {FAULT_LIMIT} times in a row")
        };
        let msg = format_plugin_disabled(name, event.into(), &why);
        rt.diagnostics.messages.push(msg);
    }
    hr
}

// ── Braked hooks ────────────────────────────────────────────────────────

/// Drive buffer-change hooks whenever the line or cursor moves.
/// Called from `Hinter::hint()`, which holds no lock; the runtime lock is
/// taken and released around each dispatch.
///
/// `pos` is the byte offset rustyline supplies; it is converted once to a
/// character offset so everything downstream (the change-detection snapshot,
/// the editor state exposed to plugins) speaks the same
/// units as the rest of the `_ed-*` surface.
pub(super) fn run_buffer_change_hooks(
    t: &dyn Transport,
    host: &Arc<ReplHost>,
    line: &str,
    pos: usize,
) {
    let pos = byte_to_char(line, pos);
    let (old, handlers) = {
        let mut rt = lock(&host.runtime);
        if line == rt.hooks.previous.text && pos == rt.hooks.previous.cursor {
            return;
        }
        let old = std::mem::replace(&mut rt.hooks.previous.text, line.to_string());
        rt.hooks.previous.cursor = pos;
        (old, rt.with_hook(HookEvent::BufferChange))
    };
    let arg = FOValue::Map {
        entries: vec![("old".into(), old.encode())],
    };
    let mut ghost: Option<String> = None;
    let mut spans: Vec<HighlightSpan> = Vec::new();
    for name in handlers {
        let hr = run_braked(t, host, &name, BUFFER_CHANGE, arg.clone(), line, pos);
        if let Some(ctx) = hr.ctx {
            ghost = ctx.outputs.ghost_text.or(ghost);
            spans.extend(ctx.outputs.highlight_spans);
        }
    }

    // With no handler left this clears the last line's ghost and highlights.
    let mut rt = lock(&host.runtime);
    rt.hooks.ghost = ghost;
    rt.hooks.highlights = spans;
}

/// The plugins' `complete` hooks, in load order, over the word at byte `pos`:
/// the first non-empty list claims it, replacing from the word's start.
pub(super) fn run_complete_hooks(
    t: &dyn Transport,
    host: &Arc<ReplHost>,
    line: &str,
    pos: usize,
) -> Option<(usize, Vec<Candidate>)> {
    let handlers = lock(&host.runtime).with_hook(HookEvent::Complete);
    if handlers.is_empty() {
        return None;
    }
    let pos = byte_to_char(line, pos);
    let at = parse_at(line, pos)?;
    let start = char_to_byte(line, at.offset);
    let arg = at.encode();
    handlers.into_iter().find_map(|name| {
        match run_braked(t, host, &name, COMPLETE, arg.clone(), line, pos).outcome {
            Outcome::Settled(FOValue::Unit) => None,
            Outcome::Settled(value) => match Vec::<Candidate>::decode(&value) {
                Ok(found) => (!found.is_empty()).then_some((start, found)),
                Err(why) => {
                    let warning = format_plugin_warning(
                        &name,
                        &format!(
                            "its complete hook returned the wrong shape ({why}); a complete hook \
                             returns a list of strings or [replacement, display] records, or `()` \
                             to pass"
                        ),
                    );
                    defer_plugin_message(&host.runtime, warning);
                    None
                }
            },
            _ => None,
        }
    })
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
    /// The plugins registering `event` whose breaker for it has not tripped,
    /// by name.
    pub(super) fn with_hook(&self, event: HookEvent) -> Vec<String> {
        self.plugins
            .iter()
            .filter(|p| {
                p.hooks.contains(&event)
                    && !p.health.get(&event).is_some_and(HookHealth::is_disabled)
            })
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

    /// Take what a load builtin says: admit a manifest, re-validated, or let a
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
        // Shadow lint: this load may kill its own bindings or, with an unguarded
        // prefix, an earlier plugin's sequence, so report a dead entry when
        // either it or its blocker is this plugin's.
        for (dead, blocker) in self.router.dead_entries() {
            if dead.plugin == name || blocker.plugin == name {
                errln!(
                    "{}",
                    format_plugin_warning(
                        &name,
                        &format!(
                            "keybinding '{}' of plugin '{}' will never fire: an unguarded '{}' \
                             binding of plugin '{}' claims it first",
                            dead.key, dead.plugin, blocker.key, blocker.plugin
                        ),
                    )
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::load::harness::dressed;
    use super::*;
    use ral_core::Value;

    /// `Keymap` names rustyline's `EditMode`, the rc's `edit_mode:` value, and
    /// the keymap for the `_ed-get` record.
    #[test]
    fn keymap_names_edit_mode_and_round_trips_as_a_datum() {
        assert_eq!(EditMode::from(Keymap::Vi), EditMode::Vi);
        assert_eq!(Keymap::decode(&Keymap::Vi.encode()), Ok(Keymap::Vi));
    }

    /// A `complete` hook claims the word: it sees the command at the cursor,
    /// and its list replaces from the word's start.
    #[test]
    fn a_complete_hook_claims_the_word() {
        let (t, host, sink) = dressed(&[(
            "p",
            HookEvent::Complete,
            "{ |at| record $at[current] $at[offset] ...$at[words]; return ['main', 'feature'] }",
        )]);
        let claim = run_complete_hooks(&t, &host, "git checkout ma", 15);
        assert_eq!(
            claim,
            Some((
                13,
                ["main", "feature"]
                    .map(|s| Candidate {
                        display: s.into(),
                        replacement: s.into()
                    })
                    .into()
            ))
        );
        assert_eq!(
            *sink.lock().unwrap(),
            vec![
                Value::Int(2),
                Value::Int(13),
                Value::string("git"),
                Value::string("checkout"),
                Value::string("ma"),
            ]
        );
    }

    /// An empty list claims nothing: the engine answers.
    #[test]
    fn an_empty_complete_list_passes_to_the_engine() {
        let (t, host, _) = dressed(&[("p", HookEvent::Complete, "{ |_at| return [] }")]);
        assert_eq!(run_complete_hooks(&t, &host, "git ", 4), None);
        assert_eq!(
            lock(&host.runtime).diagnostics.messages,
            Vec::<String>::new()
        );
    }

    /// `()` passes, silently.
    #[test]
    fn a_unit_complete_return_passes_silently() {
        let (t, host, _) = dressed(&[("p", HookEvent::Complete, "{ |_at| return () }")]);
        assert_eq!(run_complete_hooks(&t, &host, "git ", 4), None);
        assert_eq!(
            lock(&host.runtime).diagnostics.messages,
            Vec::<String>::new()
        );
    }

    /// A value of the wrong shape passes, with one warning deferred.
    #[test]
    fn a_wrong_shaped_complete_return_passes_with_a_warning() {
        let (t, host, _) = dressed(&[("p", HookEvent::Complete, "{ |_at| return 7 }")]);
        assert_eq!(run_complete_hooks(&t, &host, "git ", 4), None);
        let rt = lock(&host.runtime);
        assert_eq!(rt.diagnostics.messages.len(), 1);
        assert!(
            rt.diagnostics.messages[0].contains("its complete hook returned the wrong shape"),
            "{}",
            rt.diagnostics.messages[0]
        );
    }

    /// A record candidate keeps its display apart from its replacement.
    #[test]
    fn a_record_candidate_keeps_its_display() {
        let (t, host, _) = dressed(&[(
            "p",
            HookEvent::Complete,
            "{ |_at| return [[replacement: 'main', display: 'main (current)']] }",
        )]);
        assert_eq!(
            run_complete_hooks(&t, &host, "git ", 4),
            Some((
                4,
                vec![Candidate {
                    display: "main (current)".into(),
                    replacement: "main".into()
                }]
            ))
        );
    }

    /// A `complete` hook that always fails is disabled on its third Tab, with
    /// one notice, and is not run on the fourth.
    #[test]
    fn a_failing_complete_hook_trips_the_breaker_on_the_third_tab() {
        let (t, host, _) = dressed(&[(
            "p",
            HookEvent::Complete,
            "{ |_at| fail [status: 1, message: 'boom'] }",
        )]);
        for _ in 0..3 {
            assert_eq!(run_complete_hooks(&t, &host, "git ", 4), None);
        }
        let seen = lock(&host.runtime).diagnostics.messages.clone();
        assert_eq!(seen.len(), 4, "three faults, then the notice: {seen:?}");
        assert!(
            seen[3].contains("hook 'complete' disabled for this session"),
            "{}",
            seen[3]
        );
        assert_eq!(run_complete_hooks(&t, &host, "git ", 4), None);
        assert_eq!(lock(&host.runtime).diagnostics.messages, seen);
    }
}
