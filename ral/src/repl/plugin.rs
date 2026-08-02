//! Plugin runtime: shared mutable state threaded between the REPL loop,
//! rustyline callbacks, and plugin handler invocations.
//!
//! The `Arc<Mutex<PluginRuntime>>` lives across rustyline's `Hinter` and
//! `Highlighter` (which require `Send + Sync`) and the REPL's own
//! keybinding dispatch.  It holds the canonical plugin list (`plugins`)
//! and the `keybindings_dirty` reconciliation flag directly, and
//! partitions the rest into three named substructs so each call site
//! reaches for only the slice it owns:
//!
//! - [`EditorHooks`] — hook env, prev-buffer change-detection state,
//!   the most recent hook outputs (ghost text, highlights), the
//!   editor state exposed to plugins via `_ed-*`, and the history
//!   snapshot.  Touched by Hinter/Highlighter callbacks.
//! - [`Keybindings`] — pending keybinding flagged by rustyline's event
//!   handler, the buffer stack populated by `_ed-push`, and the key
//!   sequences currently bound with rustyline.
//! - [`DeferredDiagnostics`] — plugin error/warning messages buffered
//!   during readline for later flushing past line-erase escapes.
//!
//! One invariant is load-bearing: editor callbacks may touch this
//! mutex, but the evaluator must not.  Every hook call releases the
//! lock before invoking ral code so re-entrant `_ed-*` builtins can
//! re-acquire it.

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

use ral_core::transport::{Program, Run};
use ral_core::types::{Break, Capabilities, Mooring, Settled};
use ral_core::{
    HookName, RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin, Shell,
    StaticDiagnostics, Value, diagnostic,
};
use std::time::Duration;

use self::editor::{EditorState, HighlightSpan, PluginContext, PluginInputs, PluginOutputs};
use self::manifest::LoadedPlugin;
use super::errfmt::{format_plugin_disabled, plugin_error};
use super::frontend::EditBuffer;
use ral_core::text::byte_to_char;
// Anchored to the crate root: a bare `rustyline::` path here would resolve
// to the sibling `rustyline` module instead of the crate of the same name.
use ::rustyline::KeyEvent;
use ::rustyline::config::EditMode;
use std::sync::{Arc, Mutex, MutexGuard};

// ── Lock helper ─────────────────────────────────────────────────────────

pub(super) fn lock(m: &Arc<Mutex<PluginRuntime>>) -> MutexGuard<'_, PluginRuntime> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A plugin-load failure.  The message carries no surface tag: the display
/// site that reports it (`cmd_error`, the ralrc loader) owns the prefix,
/// so it appears exactly once.  Shared by the loader and the manifest
/// parser.
pub(super) fn load_err(msg: impl std::fmt::Display) -> ral_core::types::Error {
    ral_core::types::Error::new(msg.to_string(), 1)
}

/// An unload failure; untagged for the same reason as [`load_err`].
pub(super) fn unload_err(msg: impl std::fmt::Display) -> ral_core::types::Error {
    ral_core::types::Error::new(msg.to_string(), 1)
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
            EditMode::Vi => Self::Vi,
            _ => Self::Emacs,
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
/// `plugins` is the canonical list — owned directly, not synced from
/// any other store.  `keybindings_dirty` is set by `load` / `unload`
/// to signal the next readline iteration that rustyline should
/// re-register key handlers.
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
///
/// Use this from inside the readline loop (keybinding dispatch, buffer-change
/// hooks), where the REPL is about to emit a line-erase escape that would
/// clobber an immediate `eprintln!`.  The messages are the source-mapped hook
/// fault produced by [`call_plugin_hook`] and the circuit-breaker's disable
/// notice — both already finished strings.  One-shot lifecycle paths, where no
/// escape is pending, call [`errfmt::plugin_error`](super::errfmt::plugin_error)
/// directly instead.
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

/// Per-call view of the plugin owning a hook: `name` labels the hook run's
/// root context and the source-mapped fault.
#[derive(Clone, Copy)]
pub(super) struct HookFor<'a> {
    pub(super) name: &'a str,
}

/// Outcome of one [`call_plugin_hook`] invocation.
///
/// `ctx` is `Some` iff the caller passed `Some` for `ctx_in`; the caller
/// inspects `outputs` (ghost text, highlights, pushed buffer, accept
/// flag) and `state_cell` (to save back into the plugin record).
///
/// `rendered_error` is the source-mapped rendering of a [`Break::Error`] the
/// handler raised, produced *inside the helper* while the hook run's source
/// registry is still the live one — the next framed run resets it, so a
/// deferred raw `Error` would later resolve its `FileId` against the wrong
/// registry. It is `Some` only on the [`HookFraming::Framed`] path (where the
/// helper owns the source context); the in-frame path leaves rendering to the
/// caller against the command's own frame. `timed_out` reports whether the
/// run's wall fired — the circuit-breaker's signal that a hook overran its
/// budget.
pub(super) struct HookResult {
    pub(super) result: Settled<Value>,
    pub(super) ctx: Option<PluginContext>,
    pub(super) rendered_error: Option<String>,
    pub(super) timed_out: bool,
}

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
/// breaker. The wall is cooperative — the trampoline polls cancellation at
/// every reduction step, so any handler doing ordinary work (iteration,
/// command spawns, recursion) is preempted at the next step.
const BUFFER_CHANGE_BUDGET: Duration = Duration::from_millis(100);

/// Consecutive buffer-change faults that trip the breaker.
const BUFFER_CHANGE_FAULT_LIMIT: u32 = 3;

/// How a hook call is framed against the run machinery.
///
/// A hook handler is a thunk applied to argument values, so it runs through
/// the value run door — but only when there is no run frame around it yet.
/// The three hook contexts differ exactly here:
///
///   - [`HookFraming::Framed`] — keybinding dispatch, buffer-change, and
///     prompt hooks fire during the frontend `read`, *outside* any run frame.
///     They must establish one, so a hook that runs `_ed-tui` (a keybinding
///     handler) lands under [`RequestedTerminalAccess::Leased`] and its
///     terminal loan can elevate to foreground the body's pipeline; the others
///     pass `Denied`, since they never hand the terminal to a child.
///   - [`HookFraming::InFrame`] — lifecycle hooks (`pre-exec`, `post-exec`,
///     `chpwd`) bracket the command rather than belonging to it, so they are
///     applied in place under the caller's mooring; framing each one would
///     make it a run in its own right, with its own status and streams.
#[derive(Clone, Copy)]
pub(super) enum HookFraming<'a> {
    /// Establish a fresh run frame before applying the handler. Inherits the
    /// session streams and runs no lifecycle hooks; the [`FramedHook`] carries
    /// the rest of the per-hook policy.
    Framed(FramedHook),
    /// Apply the handler in place, under the mooring it carries.
    InFrame(&'a Mooring),
}

/// The per-hook policy for a [`HookFraming::Framed`] call.
///
/// `terminal` is the terminal authority the hook run may hand to a child
/// (keybinding dispatch leases it; the others deny it). `kind` labels the hook
/// for the run's root context and fault attribution (`"keybinding"`,
/// `"buffer-change"`, `"prompt"`). `budget` arms the run's foreground wall:
/// `Some(d)` cancels a handler that overruns `d` (the buffer-change keystroke
/// budget), `None` leaves it uncapped.
#[derive(Clone, Copy)]
pub(super) struct FramedHook {
    pub(super) terminal: RequestedTerminalAccess,
    pub(super) kind: &'static str,
    pub(super) budget: Option<Duration>,
}

/// The single primitive for running a plugin hook.  In order:
///   1. take any pre-existing `shell.repl().plugin_context` aside
///   2. install `ctx_in` (when `Some`) so `_ed-*` builtins resolve correctly
///   3. apply the handler to `args`, framed per `framing` (plugins run with
///      the host authority the framed run already carries)
///   4. take the context back out (now carrying outputs and any `state_cell` mutation)
///   5. restore the pre-existing context
///
/// The context install/restore is local-state bookkeeping that a run frame
/// does not swap, so it brackets the framing decision: a [`HookFraming::Framed`]
/// handler sees the same `plugin_context` whether the value run door installs
/// a frame or not.
///
/// State cell flows through `ctx_in.state_cell` / `result.ctx.state_cell`
/// — the helper does not touch the plugin record itself, so it works
/// uniformly whether the plugin lives in the live shell registry (for
/// lifecycle/prompt hooks) or behind the runtime mutex (for buffer-change
/// / keybinding hooks).
pub(super) fn call_plugin_hook(
    shell: &mut Shell,
    plugin: HookFor<'_>,
    hook: &HookName,
    args: &[Value],
    ctx_in: Option<PluginContext>,
    framing: HookFraming<'_>,
) -> HookResult {
    let prev = shell.repl_mut().plugin_context.take();
    if let Some(ctx) = ctx_in {
        shell.repl_mut().plugin_context = Some(Box::new(ctx));
    }
    let (result, rendered_error, timed_out) = match framing {
        HookFraming::InFrame(mooring) => {
            // Lifecycle hook: resolve the hook from the table and
            // apply it directly inside the existing command frame.
            let result = match shell.mobile().context.hooks.get(hook) {
                Some(prog) => ral_core::builtins::apply(&prog.binding.value, args, mooring, shell),
                None => Err(Break::Error(ral_core::types::Error::new(
                    format!("hook '{hook}' is not registered"),
                    1,
                ))),
            };
            (result, None, false)
        }
        HookFraming::Framed(FramedHook {
            terminal,
            kind,
            budget,
        }) => {
            // Label the root context `kind:plugin` (e.g. `keybinding:fzf`).
            let label = format!("{kind}:{}", plugin.name);
            // Encode at this edge: a plugin hook call is not itself a
            // transport door, so a non-first-order argument is this call
            // site's bug to report, not `run`'s.
            let fo_args: Result<Vec<_>, _> = args
                .iter()
                .map(ral_core::serial::FOValue::try_from)
                .collect();
            let report = match fo_args {
                Ok(fo_args) => {
                    let mut req = framed_run_request(
                        &label,
                        terminal,
                        Program::Hook {
                            name: hook.clone(),
                            args: fo_args,
                        },
                    );
                    req.run.wall = budget;
                    shell.run(req)
                }
                Err(e) => RunReport::Static {
                    diagnostics: StaticDiagnostics::Host(ral_core::types::Error::new(
                        format!("hook '{hook}' argument is not first-order: {}", e.message),
                        1,
                    )),
                },
            };
            let (result, timed_out) = match report {
                RunReport::Ran {
                    result, timed_out, ..
                } => (result, timed_out),
                RunReport::Static { diagnostics } => {
                    // A host error (hook not found, non-ground arg).
                    let msg = match diagnostics {
                        StaticDiagnostics::Host(e) => e.message,
                        _ => "unknown static diagnostic".into(),
                    };
                    (
                        Err(Break::Error(ral_core::types::Error::new(msg, 1))),
                        false,
                    )
                }
            };
            // Render the fault here, while `shell.sources()` still holds this
            // run's registry.
            let rendered_error = match &result {
                Err(Break::Error(e)) => Some(
                    diagnostic::format_runtime_error_auto(shell.sources(), e, None)
                        .trim_end()
                        .to_string(),
                ),
                _ => None,
            };
            (result, rendered_error, timed_out)
        }
    };
    let ctx = shell
        .repl_mut()
        .plugin_context
        .take()
        .and_then(|b| b.downcast::<PluginContext>().ok().map(|b| *b));
    shell.repl_mut().plugin_context = prev;
    HookResult {
        result,
        ctx,
        rendered_error,
        timed_out,
    }
}

/// The per-run policy for a run the REPL host runs from a `Value` it already
/// holds — a plugin hook, the `RAL_PROMPT` body, an rc startup block, a plugin
/// factory: the session's live streams, host authority, no limits, no surface,
/// no lifecycle. `script_name` labels the root source context; `terminal`
/// varies (keybinding dispatch leases it; every other site denies it, never
/// handing the terminal to a child); `program` is the hook the run runs.
pub(super) fn framed_run_request<'a>(
    script_name: &str,
    terminal: RequestedTerminalAccess,
    program: Program,
) -> RunRequest<'a> {
    RunRequest {
        run: Run {
            program,
            script_name: script_name.to_string(),
            caps: Capabilities::root(),
            wall: None,
            deferred_lease: None,
            worker_cap: None,
            io: RunIo::Inherit,
            terminal,
            stdin: RunStdin::Inherit,
        },
        surface: None,
        deferred: None,
        desk: None,
        nursery: None,
        lifecycle: Box::new(()),
    }
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

        let handlers: Vec<String> = rt
            .plugins
            .iter()
            .filter(|p| !p.buffer_change_health.is_disabled())
            .filter(|p| p.hooks.iter().any(|h| h == "buffer-change"))
            .map(|p| p.name.clone())
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
        drop(rt);
        (old_buf, handlers, hook_env, history, keymap)
    }; // lock released

    #[allow(
        clippy::cast_possible_wrap,
        reason = "buffer char position, far below i64::MAX"
    )]
    let pos_i = pos as i64;
    let history_list = Value::List(history.iter().cloned().map(Value::String).collect());
    let mut ghost: Option<String> = None;
    let mut spans: Vec<HighlightSpan> = Vec::new();
    for name in handlers {
        let hook = HookName::plugin(name.clone(), "buffer-change".to_string());
        let state_cell = lock(runtime).state_cell(&name);

        let args = [Value::map(vec![
            ("old_buf".into(), Value::String(old_buf.clone())),
            ("line".into(), Value::String(line.to_string())),
            ("pos".into(), Value::Int(pos_i)),
            ("history".into(), history_list.clone()),
            ("keymap".into(), Value::String(keymap.clone())),
            ("state".into(), state_cell.clone().unwrap_or(Value::Unit)),
        ])];

        let ctx_in = PluginRuntime::build_plugin_context(
            line.to_string(),
            pos,
            keymap.clone(),
            history.clone(),
            true,
            state_cell,
        );

        let hr = call_plugin_hook(
            &mut hook_env,
            HookFor { name: &name },
            &hook,
            &args,
            Some(ctx_in),
            // Buffer-change hooks fire per keystroke outside any frame and
            // never hand the terminal to a child (`_ed-tui` is forbidden here),
            // so they frame with `Denied`. The keystroke budget arms the wall,
            // and the breaker disables a persistently bad hook for the session.
            HookFraming::Framed(FramedHook {
                terminal: RequestedTerminalAccess::Denied,
                kind: "buffer-change",
                budget: Some(BUFFER_CHANGE_BUDGET),
            }),
        );

        // Surface the fault (source-mapped, rendered while its registry was
        // live), then fold the outcome into the breaker. A trip emits one
        // disable notice; thereafter this plugin's hook is skipped above.
        if let Some(rendered) = &hr.rendered_error {
            defer_plugin_message(runtime, rendered.clone());
        }
        {
            let mut rt = lock(runtime);
            if let Some(p) = rt.plugins.iter_mut().find(|p| p.name == name) {
                let tripped = p.buffer_change_health.record_outcome(
                    hr.result.is_ok(),
                    hr.timed_out,
                    BUFFER_CHANGE_FAULT_LIMIT,
                );
                if tripped {
                    let why = if hr.timed_out {
                        format!(
                            "overran its {}ms keystroke budget",
                            BUFFER_CHANGE_BUDGET.as_millis()
                        )
                    } else {
                        format!("failed {BUFFER_CHANGE_FAULT_LIMIT} times in a row")
                    };
                    let msg = format_plugin_disabled(&name, "buffer-change", &why);
                    drop(rt);
                    defer_plugin_message(runtime, msg);
                }
            }
        }

        if let Some(ctx_out) = hr.ctx {
            lock(runtime).write_back_state_cell(&name, ctx_out.state_cell.as_ref());
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

// ── Plugin lifecycle helpers ─────────────────────────────────────────────

/// Prepare the hook shell and reset per-readline state before entering readline.
///
/// The hook shell carries no `PluginContext` of its own — each
/// [`call_plugin_hook`] installs one for the duration of the call and
/// takes it back afterwards.  This keeps the persistent fields
/// (`hooks.state`, `hooks.history`) as the source of truth and lets us
/// build a fresh context per handler from them.
///
/// It is an *aside* of the session: interruptible throughout, and unable to
/// absorb an interrupt older than the hook it is about to run.
pub(super) fn prepare_hook_env(shell: &Shell, runtime: &Arc<Mutex<PluginRuntime>>, keymap: Keymap) {
    let mut rt = lock(runtime);

    rt.hooks.state = EditorState {
        text: String::new(),
        cursor: 0,
        keymap: keymap_name(keymap).into(),
    };
    rt.hooks.previous = EditBuffer::default();
    rt.hooks.ghost = None;
    rt.hooks.highlights.clear();

    // Arbitrary plugin code evaluates here, so a Ctrl-C struck during a hook
    // must reach it: the hook shell joins the session, sharing its cancel
    // root, and each hook run is stamped at its own birth — so a Ctrl-C aimed
    // at a command already in flight is older than every frame the aside will
    // mint, and stands undisturbed for the run it was aimed at.
    let mut hook_env = shell.join_session();
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

    /// Resolve a pending keybinding to its bound key notation.
    ///
    /// Lookup is by the plugin's name, the only identity stable across
    /// `unload_plugin`'s `Vec::remove`; `binding_idx` then indexes that
    /// one plugin's immutable keybinding list.  Returns `None` when the
    /// named plugin is no longer loaded — the unbind-and-ignore case.
    pub(super) fn resolve_keybinding(&self, plugin: &str, binding_idx: usize) -> Option<String> {
        let p = self.plugins.iter().find(|p| p.name == plugin)?;
        let kb = p.keybindings.get(binding_idx)?;
        Some(kb.key.clone())
    }

    /// Fetch a plugin's persistent state cell by name.
    pub(super) fn state_cell(&self, name: &str) -> Option<Value> {
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

    /// Build the per-hook [`PluginContext`] both the buffer-change and
    /// keybinding paths install around a handler call: the editor state, the
    /// history snapshot and readline flag, and the plugin's persistent state
    /// cell.  `state_default_used` tracks whether a cell was already present.
    pub(super) fn build_plugin_context(
        text: String,
        cursor: usize,
        keymap: String,
        history: Vec<String>,
        in_readline: bool,
        state_cell: Option<Value>,
    ) -> PluginContext {
        let state_default_used = state_cell.is_some();
        PluginContext {
            editor_state: EditorState {
                text,
                cursor,
                keymap,
            },
            inputs: PluginInputs {
                history_entries: history,
                in_readline,
            },
            outputs: PluginOutputs::default(),
            state_cell,
            state_default_used,
        }
    }

    /// Save a handler's (possibly mutated) state cell back into the named
    /// plugin's record.  A no-op if the plugin was unloaded mid-dispatch.
    pub(super) fn write_back_state_cell(&mut self, name: &str, state_cell: Option<&Value>) {
        if let Some(p) = self.plugins.iter_mut().find(|p| p.name == name) {
            p.state_cell = state_cell.cloned();
        }
    }
}

/// Fold a named hook over all plugins that register it, threading an
/// accumulator through each call.  The `step` closure receives `shell`,
/// a [`HookFor`] view of the plugin, the handler value, and the current
/// accumulator; it returns the next accumulator.  Typical bodies invoke
/// [`call_plugin_hook`] to install the `PluginContext` around the call —
/// `fold_hook` itself only collects handlers and threads `acc`.
pub(crate) fn fold_hook<T>(
    runtime: &Arc<Mutex<PluginRuntime>>,
    shell: &mut Shell,
    hook_name: &str,
    init: T,
    mut step: impl FnMut(&mut Shell, HookFor<'_>, &HookName, T) -> T,
) -> T {
    let entries: Vec<String> = lock(runtime)
        .plugins
        .iter()
        .filter(|p| p.hooks.iter().any(|h| h == hook_name))
        .map(|p| p.name.clone())
        .collect();

    let mut acc = init;
    for name in entries {
        let hook = HookName::plugin(name.clone(), hook_name.to_string());
        acc = step(shell, HookFor { name: &name }, &hook, acc);
    }
    acc
}

/// Run a named lifecycle hook on all plugins, passing `args` to each handler.
///
/// Lifecycle hooks (`pre-exec`, `post-exec`, `chpwd`) apply the handler in
/// place ([`HookFraming::InFrame`]) rather than framing it; the caller passes
/// the mooring the handler runs under.
pub(crate) fn run_lifecycle_hook(
    runtime: &Arc<Mutex<PluginRuntime>>,
    mooring: &Mooring,
    shell: &mut Shell,
    hook_name: &str,
    args: &[Value],
) {
    fold_hook(runtime, shell, hook_name, (), |shell, plugin, hook, ()| {
        let plugin_name = plugin.name.to_string();
        let hr = call_plugin_hook(
            shell,
            plugin,
            hook,
            args,
            None,
            HookFraming::InFrame(mooring),
        );
        if let Err(Break::Error(e)) = &hr.result {
            plugin_error(&plugin_name, &format!("hook '{hook_name}' failed"), e);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
