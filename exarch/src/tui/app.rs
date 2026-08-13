//! One [`App`] owns the tabs, viewports, prompt, and gesture state, and routes
//! the [`crate::bus::Event`] stream into scrollback blocks.

use super::banner;
use super::block::{AgentSlot, RailShape};
use super::fidelity::{self, Fidelity};
use super::gesture::GestureState;
use super::line;
use super::line::bold;
use super::login::LoginOverlay;
use super::matrix::MatrixSort;
use super::palette::{AGENT_HUES, BANNER_GOLD, BANNER_PINK};
use super::picker::Picker;
use super::prompt::PromptState;
use super::render::draw;
use super::surface::SurfaceBuffer;
use super::tabs::Tabs;
use super::terminal::Term;
use super::viewport::Viewport;
use crate::agent::resources::{BusFigures, ViewFigures, ViewportFigures};
use crate::bus::card::{RailPlace, rail_place};
use crate::bus::{AgentId, AgentState, BusReceiver, Event, Inbox, Kind};
use crate::fleet::registry::{AGENT_DEMOTE_IDLE, AgentRegistry};
use crate::provider::{Provider, Usage};

use ratatui::{
    crossterm::event::{
        KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    text::{Line, Span},
};
use std::{
    collections::HashMap,
    io::{self},
    path::{Path, PathBuf},
    time::Duration,
};

/// Rows one wheel notch scrolls; paging keys instead move a frame height,
/// measured per-keystroke off the last drawn content.
const SCROLL_STEP: isize = 3;

/// The one modal overlay that may be open at a time.
pub(super) enum Overlay {
    Picker(Picker),
    Login(LoginOverlay),
}

/// The focused tab's committed lines flow into the host terminal's native
/// scrollback; off-focus tabs accumulate locally and replay in full on focus.
pub(crate) struct App {
    pub(super) tabs: Tabs,
    pub(super) prompt_state: PromptState,
    /// Shared with the editor and the worker, which drains a non-slash prefix
    /// mid-turn (`Agent::run_batch`) and the rest at the exchange boundary
    /// (`Inbox::next_or_idle`).
    pub(super) inbox: Inbox,
    /// A clone of the handle the worker and every fork mutate, so steerability
    /// and waiting-for-input are looked up rather than pushed in each frame.
    agents: AgentRegistry,
    pub(super) total_usage: Usage,
    /// Last turn's prompt size — genai's `prompt_tokens`, which already folds
    /// the cache counts in. Overwritten, not accumulated.
    pub(super) last_input: u64,
    /// Hidden when `None` (native providers with no fetched catalog).
    pub(super) context_window: Option<u64>,
    pub(super) status_model: String,
    /// Modal in behaviour — an early-return guard in [`Self::key`] — and in
    /// rendering: drawn last, over the dimmed session.
    pub(super) overlay: Option<Overlay>,
    surface: SurfaceBuffer,
    pub(super) gesture: GestureState,
    /// A render-time projection over `tabs`, never a reshuffle of the model.
    pub(super) matrix_sort: MatrixSort,
    /// Armed by [`Self::clear`]: drops root's straggler events — tokens the
    /// worker emitted before the streaming select noticed the cancel — until
    /// the next `UserPromptEcho`. Sub-agent tabs are covered instead by the
    /// `dying` window in [`Self::handle`].
    root_clear_drain: bool,
    pub(super) cwd_basename: String,
    /// Lets `render::emit_tab_title` skip the write when the title is unchanged.
    pub(super) last_title: String,
}

impl App {
    pub fn new(
        root_id: AgentId,
        root_log_dir: &Path,
        vi: bool,
        inbox: Inbox,
        agents: AgentRegistry,
    ) -> Self {
        let tabs = Tabs::new(root_id, root_log_dir);
        let cwd_basename = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".into());
        Self {
            tabs,
            prompt_state: PromptState::new(vi),
            inbox,
            agents,
            total_usage: Usage::default(),
            surface: SurfaceBuffer::new(),
            last_input: 0,
            context_window: None,
            status_model: String::new(),
            overlay: None,
            gesture: GestureState::new(),
            matrix_sort: MatrixSort::default(),
            root_clear_drain: false,
            cwd_basename,
            last_title: String::new(),
        }
    }

    pub fn total_usage(&self) -> Usage {
        self.total_usage
    }

    /// The turn-level context-pressure floor (`0..=3`), passed into each
    /// markdown commit so a stressed turn's prose renders degraded. `0` when
    /// no context window is known.
    fn context_floor(&self) -> u8 {
        fidelity::context_floor(self.last_input, self.context_window)
    }

    /// Set the status bar label and the ctx-gauge denominator from the focused
    /// agent's provider. Call at startup and after every focus or model change.
    pub fn update_live_model(&mut self, p: &Provider) {
        let status_provider = crate::provider::provider_label(p.subscription(), p.id().label());
        // A custom provider can launch with no model at all.
        self.status_model = if p.model().is_empty() {
            format!("{status_provider} · no model — run /model")
        } else {
            // The rung in force, in the picker's own ladder label: a model the
            // catalog says takes no reasoning control reads `auto`, which is
            // what goes on the wire.
            let effort = crate::provider::effort_label(&p.tuning().effort).unwrap_or("custom");
            format!("{status_provider}/{} ({effort})", p.model())
        };
        self.context_window = crate::provider::pricing::caps_or_default(p.model()).context_window;
    }

    /// Root and any sub-agent with a registered mailbox; a dead or lingering
    /// tab is not.
    pub(super) fn is_steerable(&self) -> bool {
        let focused = self.tabs.focused();
        focused == self.tabs.root() || self.agents.mailbox(focused).is_some()
    }

    /// A dead or lingering tab has no mailbox to be busy on, so it reads as
    /// waiting.
    pub(super) fn focused_waiting(&self) -> bool {
        self.agents
            .mailbox(self.tabs.focused())
            .is_none_or(|mb| mb.waiting_for_input())
    }

    /// Idle-and-parked sub-agent tabs due to leave the TAB cycle for the matrix
    /// strip, with their idle spans — projected per frame off the registry's
    /// exchange clock, never stored. Excluding the focused id means a tab leaves
    /// the cycle only the frame after `TAB` moves off it; root never leaves.
    pub(super) fn demoted(&self) -> HashMap<AgentId, Duration> {
        let root = self.tabs.root();
        let focused = self.tabs.focused();
        self.tabs
            .matrix_rows()
            .into_iter()
            .filter_map(|(id, _)| {
                if id == root || id == focused {
                    return None;
                }
                let waiting = self
                    .agents
                    .mailbox(id)
                    .is_some_and(|mb| mb.waiting_for_input());
                let idle = self.agents.idle(id)?;
                (waiting && idle >= AGENT_DEMOTE_IDLE).then_some((id, idle))
            })
            .collect()
    }

    /// Mutable access to the active `/model` picker, for `drive_picker`.
    pub(super) fn picker_mut(&mut self) -> Option<&mut Picker> {
        match self.overlay.as_mut() {
            Some(Overlay::Picker(p)) => Some(p),
            _ => None,
        }
    }

    /// Mutable access to the active `/login` overlay, for `drive_login`.
    pub(super) fn login_mut(&mut self) -> Option<&mut LoginOverlay> {
        match self.overlay.as_mut() {
            Some(Overlay::Login(l)) => Some(l),
            _ => None,
        }
    }

    /// Settle the focused tab's state for the final frame.  The worker emits its
    /// own [`AgentState::Ready`] at every park; this covers the one boundary it
    /// cannot — the exit, where the loop is over and nothing more will arrive.
    pub fn mark_ready(&mut self) {
        let focused = self.tabs.focused();
        if let Some(vp) = self.tabs.viewport_mut(focused) {
            vp.set_state(AgentState::Ready);
        }
    }

    /// True while a time-driven visual must keep repainting with no event to
    /// drive it: the elapsed-wait bar, the tab-title spinner, or a copy toast —
    /// which needs one draw past its own expiry to erase itself, hence `margin`.
    pub(super) fn animating(&self, margin: Duration) -> bool {
        let pending = self
            .tabs
            .viewport(self.tabs.focused())
            .is_some_and(|vp| vp.state().state.pending());
        pending || self.gesture.toast_live(margin) || !self.focused_waiting()
    }

    /// Age out sub-session tabs, reset root scrollback, zero cost, redraw the
    /// banner. The workers `/clear` cancels fade out through the usual
    /// `dying`/`LINGER` path, so their viewports still reach `flush_logs`.
    pub fn clear(&mut self, info: &banner::SessionInfo<'_>, term: &mut Term) -> io::Result<()> {
        let root = self.tabs.root();
        // A tab already dying keeps its earlier death instant, so a child that
        // died just before the clear is not given a fresh full window.
        self.tabs.retire_all();
        // `route_submit` cancels the in-flight response, but the unbounded bus
        // still holds whatever the worker emitted before the streaming select
        // noticed the flag — one `wait_for_cancel` poll, ~50 ms.
        self.root_clear_drain = true;
        if let Some(vp) = self.tabs.viewport_mut(root) {
            vp.reset();
        }
        self.total_usage = Usage::default();
        self.last_input = 0;
        self.surface.clear();
        self.gesture.clear_selection();
        // Queued prompts and undrained wakeups belong to the old context.
        self.inbox.clear();
        self.banner(term, info)
    }

    /// Route one event to its viewport. `bus` is read only for
    /// `Kind::Resources`'s depth/byte figures — it is the UI thread's own
    /// receiver, so this never contends with a producer's push.
    pub fn handle(&mut self, Event { id, kind }: Event, bus: &BusReceiver) {
        // A tab in the linger window is frozen: it still renders its final frame
        // and ages out, but no further event belongs in it, so a worker
        // cancelled by `/clear` cannot paint into the rebuilt session.
        if self.tabs.is_dying(id) {
            return;
        }
        // The first `UserPromptEcho` is the genuine next prompt: disarm and let
        // it through. Everything the cancelled exchange left queued is dropped.
        if id == self.tabs.root() && self.root_clear_drain {
            let echo = matches!(kind, Kind::UserPromptEcho(_));
            self.root_clear_drain = !echo;
            if !echo {
                return;
            }
        }
        match kind {
            Kind::Born {
                log_dir,
                name,
                parent,
                branch,
            } => {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "modulus by AGENT_HUES.len() yields 0..6, fits u8"
                )]
                let agent_slot = AgentSlot((self.tabs.len() % AGENT_HUES.len()) as u8);
                self.tabs
                    .born(id, &log_dir, name, parent, branch, agent_slot);
            }
            Kind::Died => {
                let floor = self.context_floor();
                self.with_viewport(id, |vp| vp.flush_open(floor));
                // Root never enters the linger window; it outlives the session.
                self.tabs.died(id);
            }
            Kind::Usage(u) => {
                // `u.input` already folds in the cache_creation and cache_read
                // counts on every adapter, so adding them again double-counts.
                // Only root's own size belongs on the ctx gauge — a sub-agent's
                // small fresh context would clobber it until root's next turn.
                if id == self.tabs.root() {
                    self.last_input = u.input;
                }
                self.total_usage += u;
                if let Some(vp) = self.tabs.viewport_mut(id) {
                    vp.add_usage(u);
                }
            }
            // Arriving text counts against the open state, so `awaiting model`
            // carries how much has come back — a frozen count under a growing
            // clock is what a stalled stream looks like.
            Kind::Token(text) => {
                let floor = self.context_floor();
                self.with_viewport(id, |vp| {
                    vp.note_streamed(text.chars().count());
                    vp.push_token(&text, floor);
                });
            }
            Kind::Thinking(text) => {
                self.with_viewport(id, |vp| {
                    vp.note_streamed(text.chars().count());
                    vp.push_thinking(&text);
                });
            }
            Kind::Boundary => {
                let floor = self.context_floor();
                self.with_viewport(id, |vp| vp.close_boundary(floor));
            }
            // Its own block; the answer's markdown run stays a separate one.
            Kind::Reasoning { text, answer_chars } => {
                self.with_viewport(id, |vp| vp.commit_thinking(text, answer_chars));
            }
            Kind::Step { n, .. } => self.push_chrome(id, RailShape::Step, line::step(n as usize)),
            Kind::State(state) => self.with_viewport(id, |vp| vp.set_state(state)),
            // A desk verb changed the world outside the turn, so it renders as
            // an act — a barrier the coalescing projection never folds into a
            // run of observations.
            Kind::HarnessCall {
                verb,
                subject,
                payload,
                failed,
            } => {
                ral_core::dbg_trace!("tui", "HarnessCall verb={verb} payload={payload:?}");
                self.with_viewport(id, |vp| vp.push_act(verb, subject, payload, failed));
            }
            Kind::ToolCall { tool, cmd, summary } => {
                ral_core::dbg_trace!("tui", "ToolCall tool={tool} cmd={cmd:?}");
                let floor = self.context_floor();
                self.with_viewport(id, |vp| match summary {
                    // A summary marks a call worth revealing: label shut, script
                    // on a click.
                    Some(s) => vp.push_tool_call(tool, s, cmd, floor),
                    // The parse-failure sentinel makes an invisible boundary:
                    // present only so the result attaches there, never reaching
                    // back to clobber an earlier call's size bar.
                    None => {
                        vp.push_plain_call(
                            (cmd != crate::shell_eval::tools::ral::INVALID_INPUT).then_some(cmd),
                        );
                    }
                });
            }
            // The body is not rendered — the openable script is the whole of
            // what a call surfaces, and the model gets the full result through
            // history — but its line count is the most recent call's size bar.
            Kind::ToolResult(text) => {
                self.with_viewport(id, |vp| vp.set_result_size(&text));
            }
            Kind::UserPromptEcho(text) => {
                self.push_chrome(id, RailShape::Prompt, line::user_prompt(&text));
            }
            Kind::StopReason(raw) => {
                self.push_chrome(id, RailShape::Plain, line::stop_reason(&raw));
            }
            Kind::Error(msg) => self.push_chrome(id, RailShape::Error, line::error(&msg)),
            Kind::SystemNote(text) => self.push_chrome(id, RailShape::Plain, line::note(&text)),
            Kind::ContextEdited { op, by } => self.push_chrome(
                id,
                RailShape::Plain,
                line::note(&format!("[context edited: {op:?} by {by:?}]")),
            ),
            // Quiet on the rail, recorded in the trace at the emit seam. A desk
            // result is always one line, so a size bar would be constant ink.
            Kind::Nudge { .. } | Kind::HarnessResult(_) => {}
            Kind::ProviderError(error) => {
                self.push_chrome(id, RailShape::Error, line::provider_error(&error));
            }
            Kind::Stalled(error) => {
                self.push_chrome(id, RailShape::Error, line::stalled(&error));
            }
            Kind::SubagentDone {
                name,
                outcome,
                text,
                elapsed,
            } => {
                let (text, error) = outcome.breadcrumb(&text);
                // The event carries no child session id, so the child's own
                // fidelity is unreachable; the breadcrumb is root's reception
                // of the result, and no preceding `ral` call means no echo.
                let fidelity = Fidelity {
                    context: self.context_floor(),
                    echo: 0,
                };
                // Always root, whatever nesting level emitted — the trunk is
                // the permanent record of delegated work.
                let root = self.tabs.root();
                self.with_viewport(root, |vp| {
                    vp.push_subagent(name, text, error, elapsed, fidelity);
                });
            }
            // A card a kit raised through the `surface` builtin. A single-`diff`
            // card joins the patch-grouping buffer, so consecutive edits to one
            // file merge; every other card is its own block.
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
                            .absorb_patch(self.tabs.viewports_mut(), id, path, hunks);
                    }
                    Err(card) => self.with_viewport(id, |vp| vp.push_card(card)),
                }
            }
            // A detached worker's outcome, or core's ready-boundary housekeeping
            // notice. Always plain text, so never the diff path above.
            Kind::Done { card, .. } | Kind::Notice { card, .. } => {
                self.with_viewport(id, |vp| vp.push_card(card));
            }
            // The agent's card arrives with its own rows; the frontend appends
            // the accumulators it owns. Here, at the render seam, because only
            // this thread may read the tabs and viewports.
            Kind::Resources { mut card, .. } => {
                let (blocks, rows, bytes) = self
                    .tabs
                    .viewport(id)
                    .map_or((0, 0, 0), super::viewport::Viewport::probe_figures);
                let lingering = self.tabs.dying_map().len() as u64;
                let live_views = (self.tabs.len() as u64).saturating_sub(lingering);
                let dead_views = (self.tabs.viewports().len() as u64).saturating_sub(live_views);
                let frontend = crate::agent::resources::frontend_rows(
                    ViewportFigures {
                        blocks,
                        rows,
                        bytes,
                        blocks_cap: super::viewport::VIEWPORT_MAX_BLOCKS as u64,
                        rows_cap: super::viewport::VIEWPORT_MAX_ROWS as u64,
                    },
                    ViewFigures {
                        live: live_views,
                        dead: dead_views,
                        agents: live_views,
                    },
                    BusFigures {
                        depth: bus.depth() as u64,
                        bytes: bus.bytes() as u64,
                    },
                );
                card.0
                    .push(crate::agent::resources::section_mark("frontend"));
                card.0.push(crate::agent::resources::rows_mark(&frontend));
                self.with_viewport(id, |vp| vp.push_card(card));
                // The `views.dead` row is a count; this names each tombstoned
                // view's id, status, and log path.
                let tombstones = self.tabs.tombstone_lines();
                if !tombstones.is_empty() {
                    self.push_chrome(id, RailShape::Plain, tombstones);
                }
            }
            Kind::Io { event, card } => match rail_place(&event.what) {
                // A read, exec, or grep. Each lands as its own event, so a burst
                // reads as `Read…, $…, Read…, $…` clutter — the buffer collapses
                // a run, even interleaved, into one block per kind. The `card` is
                // dropped here: flush rebuilds it grouped.
                Some(RailPlace::Grouped(_)) => {
                    self.surface
                        .absorb_observation(self.tabs.viewports_mut(), id, event.what);
                }
                // A write ends the ral block, so it never buffers:
                // `with_viewport` flushes any pending run first and the write
                // lands after it on the rail.
                Some(RailPlace::Barrier) => {
                    self.with_viewport(id, |vp| vp.push_write_card(card));
                }
                // A denial — the line a reader of the rail most needs to see,
                // so it lands whole rather than dissolving into a tally.
                Some(RailPlace::Standalone) => {
                    self.with_viewport(id, |vp| vp.push_card(card));
                }
                // `decode_surface` already dropped what the rail does not draw.
                None => {}
            },
            // A pin is ambient state like `Kind::Usage`, never a scrollback
            // barrier, so it is routed directly rather than through
            // `with_viewport`, which would flush the grouping windows.
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

    /// Commit any pending grouped surfaces, then hand the session's viewport to
    /// `f`. A pending io group or `▎ diff` must land before the new block, or
    /// the merged block would appear after whatever follows it on the rail.
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

    /// A dim view-local note — a slash legend, a clipboard ack. Drawn, not
    /// recorded: unlike `Kind::SystemNote` it never becomes an event.
    pub(super) fn push_note(&mut self, id: AgentId, text: &str) {
        self.push_chrome(id, RailShape::Plain, line::note(text));
    }

    /// The UI-thread twin of `Agent::note_error`, for view commands that
    /// surface their own failures. Drawn, not recorded.
    pub(super) fn push_error(&mut self, id: AgentId, message: &str) {
        self.push_chrome(id, RailShape::Error, line::error(message));
    }
    pub fn key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        // An overlay is exclusive; its own keys are handled by `drive_picker`
        // and `drive_login`. This guard only stops a stray key leaking through.
        if self.overlay.is_some() {
            return;
        }
        let can_edit = self.is_steerable();
        // Ctrl-X opens the editor-command prefix (emacs convention): Ctrl-E
        // composes the prompt in `$EDITOR` — drained by the UI loop, which owns
        // the terminal it must suspend — and any other key cancels. The
        // widget's own Ctrl-X (cut) yields; killing stays on Ctrl-W / Ctrl-K.
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
        // Tab cycles regardless of focus; every other key reaches the textarea
        // only on an editable tab, so a lingering subagent is watch-only and the
        // global prompt stays pristine for when the user tabs home.
        match k.code {
            // Paging scrolls any tab; bare Up/Down stay bound to history below.
            KeyCode::PageUp => {
                let f = self.tabs.focused();
                self.gesture.scroll_page(self.tabs.viewports_mut(), f, -1);
            }
            KeyCode::PageDown => {
                let f = self.tabs.focused();
                self.gesture.scroll_page(self.tabs.viewports_mut(), f, 1);
            }
            // Not collapsible into a guard: with <=1 tab, Tab must be a no-op,
            // not fall through to the textarea arm below.
            #[allow(clippy::collapsible_match)]
            KeyCode::Tab => {
                if self.tabs.len() > 1 {
                    let demoted = self.demoted();
                    self.tabs.focus_next(&demoted);
                }
            }
            // Up/Down walk history only from the prompt's edge rows; mid-text in
            // a multi-line draft they fall through and move the cursor. On an
            // empty prompt, Up dequeues the whole queued run back for revision.
            KeyCode::Up if self.tabs.focused() == self.tabs.root() && k.modifiers.is_empty() => {
                if self.prompt_state.row() == 0 {
                    if !self.prompt_state.edit_queued_prompt(&self.inbox) {
                        self.prompt_state.history_prev();
                    }
                } else {
                    self.prompt_state.edit_input(k);
                }
            }
            KeyCode::Down if self.tabs.focused() == self.tabs.root() && k.modifiers.is_empty() => {
                let last_row = self.prompt_state.row_count().saturating_sub(1);
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
    /// The wheel scrolls, a left-drag selects and copies on release, a click
    /// that never dragged opens its block. Shift+left falls through to the
    /// terminal's own selection, so we never see — or fight — it.
    pub fn mouse(&mut self, me: MouseEvent) {
        self.prompt_state.clear_cx_pending();
        // Motion, wheel, and press alike, so the dial glyph brightens the
        // instant the pointer crosses a dialable block.
        self.gesture
            .update_hover(me, self.tabs.viewports(), self.tabs.focused());
        match me.kind {
            // Over a dialable block the wheel dials disclosure (up reveals) and
            // consumes the event; once clamped, or over inert chrome, it scrolls.
            MouseEventKind::ScrollUp if self.wheel_dial(1) => {}
            MouseEventKind::ScrollDown if self.wheel_dial(-1) => {}
            MouseEventKind::ScrollUp => {
                let f = self.tabs.focused();
                if let Some(vp) = self.tabs.viewport_mut(f) {
                    vp.scroll_by(-SCROLL_STEP);
                }
            }
            MouseEventKind::ScrollDown => {
                let f = self.tabs.focused();
                if let Some(vp) = self.tabs.viewport_mut(f) {
                    vp.scroll_by(SCROLL_STEP);
                }
            }
            MouseEventKind::Down(MouseButton::Left)
                if !me.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.gesture.press(me);
            }
            MouseEventKind::Drag(MouseButton::Left) => self.gesture.drag(me),
            MouseEventKind::Up(MouseButton::Left) => {
                let f = self.tabs.focused();
                self.gesture.release(self.tabs.viewports_mut(), f);
            }
            _ => {}
        }
    }

    /// Dial the hovered block by `delta`, returning whether the level actually
    /// changed. The block's whole vertical extent is the target, so the wheel
    /// dials anywhere over a coalesced run. `false` — inert chrome, a
    /// non-dialable block, one already clamped — falls through to a scroll, so
    /// a tall run never traps the wheel.
    fn wheel_dial(&mut self, delta: i8) -> bool {
        // `App::mouse` already set the hover block for this event.
        let Some(idx) = self.gesture.hover() else {
            return false;
        };
        let id = self.tabs.focused();
        let Some(vp) = self.tabs.viewport_mut(id) else {
            return false;
        };
        vp.dial_block(idx, delta)
    }

    /// Flush every viewport — live, dying, or aged-out — to its session's
    /// `user.log`. Returns the paths root first, then subagents in dispatch
    /// order, stable across runs.
    pub fn flush_logs(&mut self) -> io::Result<Vec<PathBuf>> {
        // The open markdown buffer goes first, so a trailing streamed paragraph
        // with no double-newline yet still reaches the file.
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

    /// The focused tab's latest reply as raw markdown, for `/copy`. Empty when
    /// the tab has no viewport or its last block is not prose.
    pub(in crate::tui) fn latest_reply(&self) -> String {
        self.tabs
            .viewport(self.tabs.focused())
            .map(Viewport::latest_reply_md)
            .unwrap_or_default()
    }

    /// Flush the focused tab's `user.log` and return its path for `/export`.
    /// Flushes the open markdown buffer first, as [`Self::flush_logs`] does.
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

    pub fn banner(&mut self, term: &mut Term, s: &banner::SessionInfo<'_>) -> io::Result<()> {
        // The wordmark and eagle sit outside Bertin's data variables, so this
        // alone keeps the saturated palette and carries no rail.
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
                line::render_card(&banner::session_card(s), 3),
            );
        }
        draw(self, term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::card::{Card, Mark};
    use crate::tui::line::plain;
    use crate::tui::palette::READ_W;
    use ral_core::types::{CallSite, Observation, Observed};

    fn app() -> (App, BusReceiver) {
        let (_tx, rx) = crate::bus::channel();
        let app = App::new(
            1,
            &std::env::temp_dir(),
            false,
            Inbox::new(),
            AgentRegistry::new(),
        );
        (app, rx)
    }

    fn read(path: &str) -> Kind {
        Kind::Io {
            event: Observation::instant(
                CallSite::default(),
                String::new(),
                Observed::Read { path: path.into() },
            ),
            card: Card(vec![]),
        }
    }

    fn text(app: &mut App, id: AgentId) -> String {
        let w = app
            .tabs
            .viewport_mut(id)
            .expect("the tab under test has a viewport")
            .render_window(READ_W, 40);
        w.lines.iter().map(plain).collect::<Vec<_>>().join("\n")
    }

    /// A pin is ambient state, not a scrollback barrier: arriving mid-burst it
    /// must leave the observation run whole, and land in the keyed register
    /// rather than on the rail.
    #[test]
    fn a_pin_never_splits_a_coalesced_observation_run() {
        let (mut app, rx) = app();
        for kind in [
            Kind::ToolCall {
                tool: "ral",
                cmd: "read 'a.rs'".into(),
                summary: Some("look around".into()),
            },
            read("a.rs"),
            Kind::Pin {
                key: "tasks".into(),
                card: Card(vec![Mark::Raw {
                    bytes: b"one left".to_vec(),
                }]),
            },
            read("b.rs"),
            Kind::Boundary,
        ] {
            app.handle(Event { id: 1, kind }, &rx);
        }

        let vp = app.tabs.viewport(1).expect("root has a viewport");
        assert_eq!(
            vp.probe_figures().0,
            2,
            "the call, then the two reads as one coalesced block"
        );
        assert_eq!(
            vp.pins()
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["tasks"],
            "the pin lands in the register, never in scrollback"
        );
        let all = text(&mut app, 1);
        assert!(
            all.contains("a.rs") && all.contains("b.rs"),
            "both reads render in the one block: {all:?}"
        );
    }

    /// A tab in the linger window has rendered its final frame: a straggler
    /// from a worker whose cancel it outran must not paint into it.
    #[test]
    fn a_dying_tab_admits_no_straggler() {
        let (mut app, rx) = app();
        app.handle(
            Event {
                id: 2,
                kind: Kind::Born {
                    log_dir: std::env::temp_dir(),
                    name: "helper".into(),
                    parent: 1,
                    branch: false,
                },
            },
            &rx,
        );
        for kind in [Kind::Token("alive".into()), Kind::Boundary, Kind::Died] {
            app.handle(Event { id: 2, kind }, &rx);
        }
        let blocks = app
            .tabs
            .viewport(2)
            .expect("the child keeps its viewport through the linger window")
            .probe_figures()
            .0;

        for kind in [Kind::Token("straggler".into()), Kind::Boundary] {
            app.handle(Event { id: 2, kind }, &rx);
        }

        let all = text(&mut app, 2);
        assert!(all.contains("alive"), "the final frame survives: {all:?}");
        assert!(
            !all.contains("straggler"),
            "a dying tab admits no post-mortem text: {all:?}"
        );
        assert_eq!(
            app.tabs.viewport(2).expect("viewport").probe_figures().0,
            blocks,
            "and gains no block from the events it dropped"
        );
    }
}
