//! The main TUI application state and its methods.
//!
//! One [`App`] owns the tabs, viewports, prompt, gesture state, and
//! the event-routing logic that turns a [`crate::bus::Event`] stream
//! into scrollback blocks.

use super::banner;
use super::block::{AgentSlot, RailShape};
use super::fidelity::{self, Fidelity};
use super::gesture::GestureState;
use super::line;
use super::line::{AGENT_HUES, BANNER_GOLD, BANNER_PINK, bold};
use super::matrix::MatrixSort;
use super::picker::Picker;
use super::prompt::PromptState;
use super::render::draw;
use super::surface::SurfaceBuffer;
use super::tabs::Tabs;
use super::terminal::Term;
use super::viewport::Viewport;
use crate::bus::{AgentId, Event, Inbox, Kind};
use crate::card::IoEvent;
use crate::provider::{self, Provider, Usage};
use ratatui::{
    crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    text::{Line, Span},
};
use std::{
    io::{self},
    path::{Path, PathBuf},
    time::Duration,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Rows a wheel notch moves the view; paging keys move a frame-height at
/// a time, derived per-keystroke from the last drawn content height.
const SCROLL_STEP: usize = 3;

// ---------------------------------------------------------------------------
// App struct
// ---------------------------------------------------------------------------

/// The main TUI application state.
///
/// Owns one [`Viewport`] per session and a flat list of visible tabs.
/// The currently focused tab'\''s committed lines flow into the host
/// terminal'\''s native scrollback; off-focus tabs accumulate locally
/// and replay in full when the user tabs to them.
pub(crate) struct App {
    pub(super) tabs: Tabs,
    pub(super) prompt_state: PromptState,
    /// The session'\''s own inbox, bound here by [`Self::bind_inbox`] at REPL
    /// start so the input editor, queued-user strip, and worker drive loop
    /// share one queue. A submitted prompt is pushed onto it (through a
    /// `Mailbox`); the worker drains a non-slash prefix after a tool result to
    /// steer the next assistant step ([`Agent::dispatch`]) and the remainder at
    /// the next turn boundary ([`Inbox::next_or_idle`]). Until drained, the
    /// strip renders queued user prompts, and bare Up on an empty prompt pulls
    /// the whole queued run back into the editor for revision.
    pub(super) inbox: Inbox,
    pub(super) total_usage: Usage,
    /// Last turn'\''s prompt size (genai'\''s `prompt_tokens`, which already
    /// folds the cache-read and cache-creation counts in); drives the
    /// `ctx N%` gauge.  Overwritten, not accumulated.
    pub(super) last_input: u64,
    /// Hidden when `None` (native providers with no fetched catalog).
    pub(super) context_window: Option<u64>,
    /// The live `provider model` shown in the per-frame status bar,
    /// updated on a `/model` switch. The startup banner is one-shot
    /// chrome; this is where the current model stays visible.
    pub(super) status_model: String,
    /// The active `/model` picker, taking over the prompt region while
    /// open. `None` when the prompt is the normal text editor. Modal in
    /// behaviour (an early-return guard in [`Self::key`]), flat in
    /// rendering — a strip, not a floating overlay.
    pub(super) picker: Option<Picker>,
    /// Grouped surface accumulator: patch-diff coalescing and I/O observation
    /// bucketing, moved out to [`SurfaceBuffer`].
    surface: SurfaceBuffer,
    /// Geometry of the content area, active selection, in-flight press,
    /// hover target, and copy-confirmation toast — extracted to [`gesture::GestureState`].
    pub(super) gesture: GestureState,
    /// How the multi-agent matrix orders its rows — a render-time
    /// projection of the same `tabs`/`viewports` model, never a reshuffle
    /// of the underlying state.
    pub(super) matrix_sort: MatrixSort,
    /// Set by [`Self::clear`] when the trunk viewport is blanked: drops leftover
    /// events from a turn cancelled in flight (`Token`, `Boundary`, ...) until
    /// the next prompt genuinely begins.  Only the root needs guarding --
    /// retired sub-agent tabs are already dropped in [`Self::handle`] via the
    /// `dying` linger window -- because the unbounded bus channel can still
    /// carry tokens the worker emitted between the cancel and when the
    /// streaming select notices the flag (at most one `wait_for_cancel` poll).
    /// Disarmed when the next `UserPromptEcho` arrives.
    root_clear_drain: bool,
}

impl App {
    pub fn new(
        root_id: AgentId,
        root_log_dir: &Path,
        context_window: Option<u64>,
        vi: bool,
    ) -> Self {
        let tabs = Tabs::new(root_id, root_log_dir);
        Self {
            tabs,
            prompt_state: PromptState::new(vi),
            inbox: Inbox::new(),
            total_usage: Usage::default(),
            surface: SurfaceBuffer::new(),
            last_input: 0,
            context_window,
            status_model: String::new(),
            picker: None,
            gesture: GestureState::new(),
            matrix_sort: MatrixSort::default(),
            root_clear_drain: false,
        }
    }

    pub fn total_usage(&self) -> Usage {
        self.total_usage
    }

    /// The turn-level context-pressure floor (`0..=3`), the seed signal of
    /// coherent degradation (Move 7): `last_input` against the model's
    /// `context_window`.  Passed into each markdown commit so a stressed
    /// turn's prose renders degraded; `0` when no context window is known.
    fn context_floor(&self) -> u8 {
        fidelity::context_floor(self.last_input, self.context_window)
    }

    /// Set the live `provider model` label shown in the status bar. Set
    /// once at startup and again on every `/model` switch.
    /// Set the live model from the focused agent'\''s provider.  Updates the
    /// status bar label and the context window — the denominator of the ctx%
    /// gauge — so both follow `/model` and `TAB`.  Call at startup, after
    /// every focus change, and after a model switch.
    pub fn update_live_model(&mut self, p: &Provider, status_provider: &str) {
        self.status_model = format!("{status_provider} {}", p.model());
        self.context_window = provider::caps_for(p.model()).context_window;
    }

    /// Bind the App's inbox to the session's own queue, so the input editor,
    /// queued-user strip, and worker drive loop read and write one inbox.
    /// Called once at REPL start; before it, the App holds the throwaway inbox
    /// [`App::new`] seeded for input editing.
    pub fn bind_inbox(&mut self, inbox: Inbox) {
        self.inbox = inbox;
    }

    /// Mutable access to the active picker, for the REPL's picker loop.
    pub(super) fn picker_mut(&mut self) -> Option<&mut Picker> {
        self.picker.as_mut()
    }

    pub fn busy_off(&mut self) {
        // A turn ending supersedes any live phase label: clear it on the
        // focused viewport so the elapsed-wait bar disappears.
        let focused = self.tabs.focused();
        if let Some(vp) = self.tabs.viewport_mut(focused) {
            vp.clear_phase();
        }
    }

    /// True while a time-driven visual is live and must keep repainting on
    /// its own — the UI loop redraws on this even when no bus or input event
    /// arrived. Covers the focused tab's elapsed-wait bar (ticks with wall
    /// time while a phase is in flight), a live copy toast (needs one more
    /// draw right after its own expiry to erase itself, hence `margin`), and
    /// the terminal tab-title spinner (rotates only while the trunk is
    /// working, and has no bus event of its own to announce a tick).
    pub(super) fn animating(&self, margin: Duration) -> bool {
        let phase_live = self
            .tabs
            .viewport(self.tabs.focused())
            .is_some_and(|vp| vp.phase_label().is_some());
        phase_live || self.gesture.toast_live(margin) || !self.inbox.waiting_for_input()
    }

    /// Age out sub-session tabs, reset root scrollback, zero cost, redraw the
    /// banner.  A `/clear` cancels every live background worker and bumps the
    /// registry generation; here the frontend twin retires their tabs through
    /// the existing `dying`/`LINGER` path rather than dropping them abruptly,
    /// so a worker cancelled across the context rebuild fades out instead of
    /// vanishing — and the [`Self::handle`] dying-guard stops it painting into
    /// the rebuilt session in the meantime.  `tick` then reaps the faded tabs
    /// (their viewports persist for `flush_logs`, exactly as a naturally-dead
    /// child's do).
    pub fn clear(
        &mut self,
        info: &banner::SessionInfo<'_>,
        p: &Provider,
        term: &mut Term,
    ) -> io::Result<()> {
        let root = self.tabs.root();
        // Retire every still-live non-root tab into the linger window. A tab
        // already dying keeps its earlier death instant, so a child that died
        // just before the clear is not given a fresh full window.
        self.tabs.retire_all();
        // A `/clear` on the trunk cancels an in-flight model response in
        // `route_submit`; the cancel trips within one `wait_for_cancel` poll
        // (~50 ms), but the unbounded bus can still carry tokens the worker
        // emitted before the streaming select noticed the flag.  Until the
        // next prompt echoes genuinely, drop those stragglers in
        // [`Self::handle`].
        self.root_clear_drain = true;
        if let Some(vp) = self.tabs.viewport_mut(root) {
            vp.reset();
        }
        self.total_usage = Usage::default();
        self.last_input = 0;
        self.surface.clear();
        self.gesture.clear_selection();
        // A fresh root: drop queued user prompts and any stale non-human
        // deliveries (a wakeup or agent result that has not been drained).
        self.inbox.clear();
        self.banner(term, info, p)
    }

    /// Route one event to its viewport.  Born registers a pane; Died
    /// flushes; Usage accumulates globally; everything else renders to
    /// one viewport via [`line`](mod@line).
    pub fn handle(&mut self, Event { id, kind }: Event) {
        // A tab in the linger window is frozen: its worker has emitted `Died`
        // (natural death) or been retired by `/clear` (a cancelled background
        // worker still winding down).  Either way no further event belongs in
        // it — dropping them here stops a cancelled worker painting into the
        // rebuilt session, the visual twin of the inbox's stale-generation
        // rejection, while the tab still renders its final frame and ages out.
        // Root never enters `dying`, so its events always pass.
        if self.tabs.is_dying(id) {
            return;
        }
        // While the trunk viewport is freshly cleared (`App::clear` armed
        // `root_clear_drain`), drop the straggler events the cancelled turn
        // left in the unbounded bus -- the tokens and trailing chrome the
        // worker emitted before the streaming `select!` noticed the cancel
        // flag, at most one `wait_for_cancel` poll (~50 ms) of queued events.
        // The first `UserPromptEcho` is the genuine next prompt: disarm the
        // guard and let it through unchanged.  A `Born`/`Died` carries a
        // sub-agent own id, never root, so the dying guard above owns them;
        // for root we drop the lot.
        if id == self.tabs.root() && self.root_clear_drain {
            let echo = matches!(kind, Kind::UserPromptEcho(_));
            self.root_clear_drain = !echo;
            if !echo {
                return;
            }
        }
        // A phase label names the silent gap before the next thing
        // happens, so any other event supersedes it.  Clear the live
        // phase on the event's viewport first, resetting the elapsed-wait
        // bar so it tracks only the gap before the *next* phase.
        if !matches!(kind, Kind::Phase(_))
            && let Some(vp) = self.tabs.viewport_mut(id)
        {
            vp.clear_phase();
        }
        match kind {
            Kind::Born {
                log_dir,
                title,
                parent,
            } => {
                let agent_slot = AgentSlot((self.tabs.len() as u8) % AGENT_HUES.len() as u8);
                self.tabs.born(id, &log_dir, title, parent, agent_slot);
            }
            Kind::Died => {
                self.surface.flush_surfaces(self.tabs.viewports_mut());
                let floor = self.context_floor();
                if let Some(vp) = self.tabs.viewport_mut(id) {
                    vp.flush_open(floor);
                }
                // Root never enters the linger window; it lives as
                // long as the program does.
                self.tabs.died(id);
            }
            Kind::Usage(u) => {
                // `u.input` (genai's `prompt_tokens`) already folds in the
                // cache_creation and cache_read counts on every adapter, so
                // adding them again double-counts the prompt — ~2x on a
                // cache-heavy session, on the one gauge that tells the user
                // when to `/compact` (X4).  Take the prompt total as-is.
                // Only the root's own prompt size belongs on that gauge; a
                // concurrently-running sub-agent's small fresh-context usage
                // would otherwise clobber it until the root's next round-trip.
                if id == self.tabs.root() {
                    self.last_input = u.input;
                }
                self.total_usage += u;
                if let Some(vp) = self.tabs.viewport_mut(id) {
                    vp.add_usage(u);
                }
            }
            Kind::Token(text) => {
                self.surface.flush_surfaces(self.tabs.viewports_mut());
                let floor = self.context_floor();
                if let Some(vp) = self.tabs.viewport_mut(id) {
                    vp.push_token(&text, floor);
                }
            }
            Kind::Thinking(text) => {
                self.with_viewport(id, |vp| vp.push_thinking(&text));
            }
            Kind::Boundary => {
                self.surface.flush_surfaces(self.tabs.viewports_mut());
                let floor = self.context_floor();
                if let Some(vp) = self.tabs.viewport_mut(id) {
                    vp.close_boundary(floor);
                }
            }
            // Final reasoning is its own block; the answer's markdown run
            // remains a separate `·` block.
            Kind::Reasoning { text, answer_chars } => {
                self.with_viewport(id, |vp| vp.commit_thinking(text, answer_chars));
            }
            Kind::Step { n, .. } => self.push_chrome(id, RailShape::Step, line::step(n as usize)),
            // Route to the event's viewport; `set_phase` restarts the
            // elapsed-wait clock, so a consecutive Phase event simply
            // resets the bar to the new phase.
            Kind::Phase(label) => self.with_viewport(id, |vp| vp.set_phase(label)),
            Kind::ToolCall { tool, cmd, summary } => {
                ral_core::dbg_trace!("tui", "ToolCall tool={tool} cmd={cmd:?}");
                let floor = self.context_floor();
                self.with_viewport(id, |vp| match summary {
                    // A summary marks a call worth revealing: the label
                    // shows shut, the script on a click.
                    Some(s) => vp.push_tool_call(tool, s, cmd, floor),
                    // A summary-less call is a plain tool call, shown standalone.
                    // Its cmd being the parse-failure sentinel makes it an invisible
                    // boundary (`None`): present only so its result attaches there,
                    // never reaching back to clobber an earlier call's size bar.
                    None => {
                        vp.push_plain_call(tool, (cmd != crate::tools::INVALID_INPUT).then_some(cmd))
                    }
                });
            }
            // A tool result's body is not rendered — the script the user
            // can open is the whole of what a call surfaces, and the model
            // receives the full result through the history pipeline — but
            // its line count is the call's magnitude, attached to the
            // most-recent tool-call block as the collapsed header's
            // size-bar.
            Kind::ToolResult(text) => self.with_viewport(id, |vp| vp.set_result_size(&text)),
            Kind::UserPromptEcho(text) => {
                self.push_chrome(id, RailShape::Prompt, line::user_prompt(&text))
            }
            Kind::StopReason(raw) => {
                self.push_chrome(id, RailShape::Plain, line::stop_reason(&raw))
            }
            Kind::Error(msg) => self.push_chrome(id, RailShape::Error, line::error(&msg)),
            Kind::SystemNote(text) => self.push_chrome(id, RailShape::Plain, line::note(&text)),
            // Quiet on the rail; recorded in the trace at the emit seam.
            Kind::Nudge { .. } => {}
            Kind::ProviderError(error) => {
                self.push_chrome(id, RailShape::Error, line::provider_error(&error))
            }
            Kind::SubagentDone {
                title,
                outcome,
                text,
                elapsed,
            } => {
                let (text, error) = outcome.breadcrumb(&text);
                // The event carries no child session id, so the child's own
                // per-block fidelity is unreachable here; the breadcrumb is
                // root's reception of the result, so it degrades with root's
                // turn-level context floor (echo does not apply — there is
                // no preceding `ral` call in this render context).
                let fidelity = Fidelity {
                    context: self.context_floor(),
                    echo: 0,
                };
                // Always lands in root, regardless of which nesting
                // level emitted — main is the permanent record of
                // delegated work.
                let root = self.tabs.root();
                self.with_viewport(root, |vp| {
                    vp.push_subagent(title, text, error, elapsed, fidelity)
                });
            }
            // A surfaced render document: a kit raised a card through the
            // `surface` builtin — a deliberate choice to communicate with the
            // user.  A single-`diff` card joins the patch-grouping buffer so
            // consecutive edits to one file merge into one block, the way a
            // unified diff presents one file; every other card is its own
            // scrollback block.
            Kind::Card(card) => {
                ral_core::dbg_trace!(
                    "tui",
                    "Card id={id} viewports={:?} focus={} diff={}",
                    self.tabs.viewport_keys(),
                    self.tabs.focused(),
                    card.single_diff().is_some()
                );
                match card.into_single_diff() {
                    Ok((path, hunks)) => {
                        self.surface
                            .absorb_patch(self.tabs.viewports_mut(), id, path, hunks)
                    }
                    Err(card) => self.with_viewport(id, |vp| vp.push_card(card)),
                }
            }
            // The lease chain reaped a worker at the ready boundary: its
            // one-line card lands as its own scrollback block, same as any
            // other surfaced card — but never through the diff-detection
            // path above, since a reap's card is always plain text.
            Kind::WorkerReaped { card, .. } => {
                self.with_viewport(id, |vp| vp.push_card(card));
            }
            // The binding-lease chain pruned idle top-level names at the
            // ready boundary: same one-line-card-as-scrollback-block
            // treatment as a worker reap, its sibling lease.
            Kind::BindingsPruned { card, .. } => {
                self.with_viewport(id, |vp| vp.push_card(card));
            }
            // The `/resources` fold: the agent's card arrives carrying its
            // own rows; the frontend appends the rows for the accumulators
            // *it* owns — the probed agent's viewport figures, the fleet's
            // view counts, and the bus — before the card lands.  Appended
            // here, at the render seam, because only this thread may read
            // the tabs/viewport structures; the agent's transcript keeps
            // the agent rows, and these stay presentation.
            Kind::Resources { card, .. } => {
                let mut card = card;
                let (blocks, rows, bytes) = self
                    .tabs
                    .viewport(id)
                    .map(|vp| vp.probe_figures())
                    .unwrap_or((0, 0, 0));
                let lingering = self.tabs.dying_map().len() as u64;
                let live_views = (self.tabs.len() as u64).saturating_sub(lingering);
                let dead_views = (self.tabs.viewports().len() as u64).saturating_sub(live_views);
                let frontend = crate::resources::frontend_rows(
                    blocks, rows, bytes, live_views, dead_views, live_views,
                );
                card.0.push(crate::resources::section_mark("frontend"));
                card.0.push(crate::resources::rows_mark(&frontend));
                self.with_viewport(id, |vp| vp.push_card(card));
            }
            // A write surfaced: a barrier that ends the ral block, landed
            // standalone as its own card — the `write <path> <outcome>` heading
            // plus a preview of what it wrote (composed at the emit seam in
            // `io_card`).  It never buffers; `with_viewport` flushes any pending
            // observation run first so the write lands after it on the rail.
            Kind::Io {
                event: IoEvent::Write { .. },
                card,
            } => {
                self.with_viewport(id, |vp| vp.push_write_card(card));
            }
            // An observation effect surfaced: a read, exec, or grep.  Each lands
            // as its own `Kind::Io`, so a burst reads as `Read…, $…, Read…, $…`
            // clutter — the io buffer collapses a run (even interleaved) into one
            // block per kind, flushed at the next boundary.  The per-event `card`
            // is dropped on the render path; it is reconstructed grouped at
            // flush, and the structured per-event record already reached the
            // transcript at the emit seam (`Emitter::emit`), upstream of this UI
            // handler, so nothing is lost.
            Kind::Io { event, .. } => {
                self.surface
                    .absorb_observation(self.tabs.viewports_mut(), id, event)
            }
            // Pinned state: write or drop a register slot in place.  Routed
            // directly, *not* through `with_viewport` — a pin is ambient state
            // like `Kind::Usage`, never a scrollback barrier, so it must not
            // flush the io/patch grouping windows the way a landing block does.
            Kind::Pin { key, card } => {
                if let Some(vp) = self.tabs.viewport_mut(id) {
                    vp.set_pin(key, card);
                }
            }
            Kind::Unpin { key } => {
                if let Some(vp) = self.tabs.viewport_mut(id) {
                    vp.drop_pin(&key);
                }
            }
        }
    }

    /// Commit any pending grouped surfaces, then hand the session's viewport
    /// to `f`.  Any other content closes both grouping windows: a pending io
    /// group or `▎ diff` must land before the new block, or the merged block
    /// would appear *after* whatever follows it on the rail.
    fn with_viewport(&mut self, id: AgentId, f: impl FnOnce(&mut Viewport)) {
        self.surface.flush_surfaces(self.tabs.viewports_mut());
        match self.tabs.viewport_mut(id) {
            Some(vp) => f(vp),
            None => {
                ral_core::dbg_trace!(
                    "tui",
                    "viewport event DROPPED — no viewport for id={id}; known={:?}",
                    self.tabs.viewport_keys()
                );
            }
        }
    }

    pub(super) fn push_chrome(&mut self, id: AgentId, shape: RailShape, lines: Vec<Line<'static>>) {
        self.with_viewport(id, |vp| vp.push_chrome(shape, lines));
    }

    /// Draw a dim UI note straight to the viewport — view-local chrome (a slash
    /// legend row, a clipboard or export ack) that names nothing about the run,
    /// so it is *drawn, not recorded*: it never becomes an event, the way the
    /// rendered `Kind::SystemNote` does at the emit seam.
    pub(super) fn push_note(&mut self, id: AgentId, text: String) {
        self.push_chrome(id, RailShape::Plain, line::note(&text));
    }

    /// Draw an error line straight to the viewport — the UI-thread twin of
    /// [`Agent::note_error`], for the view commands that surface their own
    /// failures.  Drawn, not recorded.
    pub(super) fn push_error(&mut self, id: AgentId, message: String) {
        self.push_chrome(id, RailShape::Error, line::error(&message));
    }
    pub fn key(&mut self, k: KeyEvent, can_edit: bool) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        // The `/model` picker is modal: while it is open no key reaches the
        // textarea or the scrollback. Its own key handling runs in the
        // UI loop's picker loop ([`drive_picker`]), which drives the
        // picker directly; this guard only keeps a stray key (e.g. one
        // arriving on a non-prompt path) from leaking through.
        if self.picker.is_some() {
            return;
        }
        // Ctrl-X opens the editor-command prefix (emacs convention).  The next
        // key completes the chord: Ctrl-E composes the prompt in `$EDITOR` (the
        // request is drained by the UI loop, which owns the terminal it must
        // suspend); any other key cancels.  The widget's own Ctrl-X (cut) yields
        // to the prefix — killing stays on Ctrl-W / Ctrl-K.
        if self.prompt_state.take_cx_pending() {
            if can_edit
                && k.code == KeyCode::Char('e')
                && k.modifiers.contains(KeyModifiers::CONTROL)
            {
                self.prompt_state.request_editor();
            }
            return;
        }
        if can_edit && k.code == KeyCode::Char('x') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.prompt_state.set_cx_pending();
            return;
        }
        // Tab cycles regardless of focus; every other key is delivered to
        // the textarea only on an editable tab (`can_edit`) — root, or a live
        // peer the caller resolved a steering mailbox for.  A dead/lingering
        // subagent tab is watch-only, keeping the global textarea pristine for
        // when the user tabs home.
        match k.code {
            // Paging scrolls the focused pane on any tab; bare Up/Down
            // stay bound to prompt history below.
            KeyCode::PageUp => {
                let f = self.tabs.focused();
                self.gesture.scroll_page(self.tabs.viewports_mut(), f, -1);
            }
            KeyCode::PageDown => {
                let f = self.tabs.focused();
                self.gesture.scroll_page(self.tabs.viewports_mut(), f, 1);
            }
            // Not collapsible into a match guard: with <=1 tab, Tab must
            // be a no-op, not fall through to the textarea-input arm below.
            #[allow(clippy::collapsible_match)]
            KeyCode::Tab => {
                if self.tabs.len() > 1 {
                    self.tabs.focus_next();
                }
            }
            // Up/Down walk the prompt history, but only from the
            // prompt's edge rows: with the cursor mid-text in a
            // multi-line draft they fall through and move the cursor.
            // When the prompt is empty and prompts are queued above it,
            // Up pulls the entire queued run back down into the editor,
            // dequeueing all of them so the user can revise the whole batch.
            KeyCode::Up if self.tabs.focused() == self.tabs.root() && k.modifiers.is_empty() => {
                if self.prompt_state.row() == 0 {
                    if !self.prompt_state.edit_queued_prompt(&mut self.inbox) {
                        self.prompt_state.history_prev();
                    }
                } else {
                    self.prompt_state.edit_input(k);
                }
            }
            KeyCode::Down if self.tabs.focused() == self.tabs.root() && k.modifiers.is_empty() => {
                let last_row = self.prompt_state.row_count() - 1;
                if self.prompt_state.row() == last_row {
                    self.prompt_state.history_next();
                } else {
                    self.prompt_state.edit_input(k);
                }
            }
            _ if can_edit => {
                self.prompt_state.edit_input(k);
            }
            _ => {}
        }
    }
    /// Route a mouse event: the wheel scrolls, a left-drag selects (and
    /// copies on release), and a left click that never dragged opens the
    /// block it landed on.  Shift+left falls through to the terminal's
    /// own selection, so we never see — or fight — it.
    pub fn mouse(&mut self, me: MouseEvent) {
        self.prompt_state.clear_cx_pending();
        // Refresh the hover mark on every event — motion, wheel, or press —
        // so the brightened dial glyph tracks the pointer the instant it
        // crosses a dialable block.
        self.gesture.set_hover(self.gesture.hover_block(
            me,
            self.tabs.viewports(),
            self.tabs.focused(),
        ));
        match me.kind {
            // Anywhere over a dialable block, the wheel dials its disclosure
            // level (up reveals, down reduces) and consumes the event; once
            // the level clamps — or over inert chrome — it scrolls instead.
            MouseEventKind::ScrollUp if self.wheel_dial(me, 1) => {}
            MouseEventKind::ScrollDown if self.wheel_dial(me, -1) => {}
            MouseEventKind::ScrollUp => {
                let f = self.tabs.focused();
                self.gesture
                    .scroll(self.tabs.viewports_mut(), f, -(SCROLL_STEP as isize));
            }
            MouseEventKind::ScrollDown => {
                let f = self.tabs.focused();
                self.gesture
                    .scroll(self.tabs.viewports_mut(), f, SCROLL_STEP as isize);
            }
            MouseEventKind::Down(MouseButton::Left)
                if !me.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                let f = self.tabs.focused();
                self.gesture.press(me, self.tabs.viewports(), f)
            }
            MouseEventKind::Drag(MouseButton::Left) => self.gesture.drag(me),
            MouseEventKind::Up(MouseButton::Left) => {
                let f = self.tabs.focused();
                self.gesture.release(self.tabs.viewports_mut(), f);
            }
            _ => {}
        }
    }

    /// Dial the dialable block under a wheel event by `delta`, returning
    /// whether it dialed — `true` only when the level actually changed.  The
    /// whole vertical extent of the block is the target (the region the
    /// hover glyph lights), so the wheel dials anywhere over a coalesced
    /// run, not just on its rail.  A wheel over inert chrome, over a
    /// non-dialable block, or over a block already clamped at the requested
    /// end returns `false` and falls through to a viewport scroll — so a
    /// tall run never traps the wheel.
    fn wheel_dial(&mut self, me: MouseEvent, delta: i8) -> bool {
        let Some(idx) = self
            .gesture
            .hover_block(me, self.tabs.viewports(), self.tabs.focused())
        else {
            return false;
        };
        let id = self.tabs.focused();
        let Some(vp) = self.tabs.viewport_mut(id) else {
            return false;
        };
        vp.dial_block(idx, delta)
    }

    /// Walk every viewport (live, dying, or aged-out) and flush its
    /// rendered-text accumulator to that session's `user.log`.
    /// Returns the list of paths, root first, then subagents in
    /// dispatch order — stable across runs for testing.
    pub fn flush_logs(&mut self) -> io::Result<Vec<PathBuf>> {
        // Flush the open markdown buffer first so any trailing
        // streamed paragraph (no double-newline yet) reaches
        // `committed`, and the `user.log`, before the final flush.
        let floor = self.context_floor();
        for vp in self.tabs.viewports_mut().values_mut() {
            vp.flush_open(floor);
        }
        let mut paths = Vec::with_capacity(self.tabs.dispatch_order().len());
        let order = self.tabs.dispatch_order().to_vec();
        for &id in &order {
            if let Some(vp) = self.tabs.viewport_mut(id) {
                paths.push(vp.flush_log()?.to_path_buf());
            }
        }
        Ok(paths)
    }

    /// The focused tab's latest assistant reply as raw markdown — the
    /// trailing run of prose blocks (see [`Viewport::latest_reply_md`]).
    /// Empty when the tab has no viewport or its last block is not prose.
    /// `/copy` reads this for the focused tab.
    pub(in crate::tui) fn latest_reply(&self) -> String {
        self.tabs
            .viewport(self.tabs.focused())
            .map(Viewport::latest_reply_md)
            .unwrap_or_default()
    }

    /// Flush the focused tab's `user.log` and return its path, so `/export`
    /// can copy the rendered transcript elsewhere.  Flushes the open
    /// markdown buffer first, mirroring [`Self::flush_logs`], so a trailing
    /// streamed paragraph reaches the file before the copy.
    pub(in crate::tui) fn flush_focused_log(&mut self) -> io::Result<PathBuf> {
        let focused = self.tabs.focused();
        let floor = self.context_floor();
        let vp = self
            .tabs
            .viewport_mut(focused)
            .expect("focused tab always has a viewport");
        vp.flush_open(floor);
        Ok(vp.flush_log()?.to_path_buf())
    }

    pub fn banner(
        &mut self,
        term: &mut Term,
        s: &banner::SessionInfo<'_>,
        p: &Provider,
    ) -> io::Result<()> {
        // The wordmark + eagle: a branded splash, an image outside Bertin's
        // data variables, so it alone keeps the saturated palette and reads
        // as neon. It carries no rail — it is not a row on the plane.
        let mut splash: Vec<Line<'static>> = vec![Line::default()];
        for (a, e) in banner::ART.lines().zip(banner::EAGLE.lines()) {
            splash.push(Line::from(vec![
                bold(a.to_string(), BANNER_PINK),
                Span::raw("  "),
                bold(e.to_string(), BANNER_GOLD),
            ]));
        }

        if let Some(vp) = self.tabs.viewport_mut(self.tabs.root()) {
            vp.push_chrome(RailShape::Plain, splash);
            vp.push_chrome(
                RailShape::Plain,
                line::render_card(&banner::session_card(s, p), 3),
            );
        }
        draw(self, term)
    }
}
