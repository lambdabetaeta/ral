//! One [`App`] owns the tabs, viewports, prompt, and gesture state, and folds
//! the [`crate::bus::Signal`] stream — [`Signal::Fact`] through [`Self::fact`],
//! [`Signal::Transient`] through [`Self::transient`] — into scrollback blocks.

use super::banner;
use super::block::{AgentSlot, RailShape};
use super::gesture::GestureState;
use super::line;
use super::line::bold;
use super::login::LoginOverlay;
use super::matrix::MatrixSort;
use super::palette::{AGENT_HUES, BANNER_GOLD, BANNER_PINK};
use super::picker::Picker;
use super::prompt::PromptState;
use super::render::draw;
use super::tabs::Tabs;
use super::terminal::Term;
use super::viewport::Viewport;
use crate::agent::resources::{BusFigures, ViewFigures, ViewportFigures};
use crate::bus::{AgentId, AgentState, BusReceiver, Inbox};
use crate::fleet::registry::{AGENT_DEMOTE_IDLE, AgentRegistry};
use crate::provider::identity::Account;
use crate::provider::{Provider, Usage};
use crate::record::{Display, Forensic, Printer as _, Record, Recorded, Transient};

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
    /// mid-turn (`Avatar::run_batch`) and the rest at the exchange boundary
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
    pub(super) gesture: GestureState,
    /// A render-time projection over `tabs`, never a reshuffle of the model.
    pub(super) matrix_sort: MatrixSort,
    /// Armed by [`Self::clear`]: drops root's straggler events — tokens the
    /// worker emitted before the streaming select noticed the cancel — until
    /// the clear acknowledgement. Sub-agent tabs are covered instead by the
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
        append_log: bool,
        inbox: Inbox,
        agents: AgentRegistry,
    ) -> Self {
        let tabs = Tabs::new(root_id, root_log_dir, append_log);
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

    /// Set the status bar label and the ctx-gauge denominator from the focused
    /// agent's provider. Call at startup and after every focus or model change.
    /// `accounts` is the set the label is drawn relative to, so two logins on
    /// one email still read apart on the status line.
    pub fn update_live_model(&mut self, p: &Provider, accounts: &[Account]) {
        let status_provider = crate::provider::identity::label(p.account(), accounts);
        // A declared service can launch with no model at all.
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
        // Feeds `Viewport::sync`'s own fidelity recomputation once the live
        // seam drives it as a `record::Printer`; harmless to set today.
        for vp in self.tabs.viewports_mut().values_mut() {
            vp.set_context_window(self.context_window);
        }
    }

    /// Root and any sub-agent still live; a dead or lingering tab is not.
    pub(super) fn is_steerable(&self) -> bool {
        let focused = self.tabs.focused();
        focused == self.tabs.root() || self.agents.by_id(focused).is_some()
    }

    /// A dead or lingering tab has no mailbox to be busy on, so it reads as
    /// waiting.
    pub(super) fn focused_waiting(&self) -> bool {
        self.agents
            .by_id(self.tabs.focused())
            .is_none_or(|agent| agent.mailbox().waiting_for_input())
    }

    /// Idle-and-parked sub-agent tabs due to leave the TAB cycle for the matrix
    /// strip, with their idle spans — projected per frame off each agent's own
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
                let agent = self.agents.by_id(id)?;
                let idle = agent.idle();
                (agent.mailbox().waiting_for_input() && idle >= AGENT_DEMOTE_IDLE)
                    .then_some((id, idle))
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
        self.gesture.clear_selection();
        // Queued prompts and undrained wakeups belong to the old context.
        self.inbox.clear();
        self.banner(term, info)
    }

    /// The dying-tab guard and the `/clear` drain gate, shared by every
    /// entry point a `Signal` reaches `App` through ([`Self::handle`],
    /// [`Self::fact`], [`Self::transient`]).
    ///
    /// A tab in the linger window is frozen: it still renders its final frame
    /// and ages out, but no further event belongs in it, so a worker
    /// cancelled by `/clear` cannot paint into the rebuilt session. And past
    /// a `/clear`, everything before the acknowledgement is cancelled-exchange
    /// residue, everything after it is new context — `disarm` is whether this
    /// particular occurrence is that acknowledgement (`Transient::Cleared`, or
    /// a fresh prompt arriving as a `Display::Prompt` fact when the ack itself
    /// was lost).
    fn admits(&mut self, id: AgentId, disarm: bool) -> bool {
        if self.tabs.is_dying(id) {
            return false;
        }
        if id == self.tabs.root() && self.root_clear_drain {
            if disarm {
                self.root_clear_drain = false;
            } else {
                return false;
            }
        }
        true
    }

    /// Fold one witnessed record fact into the screen — the sole way a
    /// `Display`/`Forensic` commit reaches it.  Steps the recording
    /// viewport's own fold-memo and re-syncs from it
    /// ([`Viewport::commit_fact`]); [`Display::SubagentDone`] always lands in
    /// root's scrollback, whatever nesting depth drained the result, since
    /// the trunk is the permanent record of delegated work.
    pub fn fact(&mut self, id: AgentId, rec: &Recorded<Record>) {
        if !self.admits(
            id,
            matches!(rec.value(), Record::Display(Display::Prompt { .. })),
        ) {
            return;
        }
        // Bookkeeping the view fold does not itself keep: the richer
        // `Usage` (dollars, cache) `App`/`Viewport` track for the status
        // line and the matrix, where `Blocks` only sums plain token counts.
        if let Record::Forensic(Forensic::UsageDelta { usage }) = rec.value() {
            let u = Usage::from(usage);
            if id == self.tabs.root() {
                self.last_input = u.input;
            }
            self.total_usage += u;
            if let Some(vp) = self.tabs.viewport_mut(id) {
                vp.add_usage(u);
            }
        }
        let target = match rec.value() {
            Record::Display(Display::SubagentDone { .. }) => self.tabs.root(),
            _ => id,
        };
        self.with_viewport(target, |vp| vp.commit_fact(rec));
    }

    /// Draw one live-only transient directly, with no log-backed fold: the
    /// mirror of [`Self::fact`] for [`crate::bus::Signal::Transient`].
    /// [`Transient::Born`]/[`Died`]/[`Resources`] need the tabs a bare
    /// `Viewport` cannot see, so they are answered here; everything else
    /// forwards to [`record::Printer::transient`] on the recording viewport.
    pub fn transient(&mut self, id: AgentId, t: Transient, bus: &BusReceiver) {
        // A `Cleared` answering *our* `/clear` is the gate's key and nothing
        // more: [`Self::clear`] already blanked the viewport and redrew the
        // banner at the keystroke, and the drain kept the interval empty, so
        // wiping again here would only cost the banner.  A `Cleared` this
        // frontend did not author finds no armed gate and blanks as ever.
        let cleared = matches!(t, Transient::Cleared);
        let ours = cleared && id == self.tabs.root() && self.root_clear_drain;
        if !self.admits(id, cleared) || ours {
            return;
        }
        match t {
            Transient::Born {
                log_dir,
                name,
                parent,
            } => {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "modulus by AGENT_HUES.len() yields 0..6, fits u8"
                )]
                let agent_slot = AgentSlot((self.tabs.len() % AGENT_HUES.len()) as u8);
                self.tabs.born(id, &log_dir, name, parent, agent_slot);
            }
            // Root never enters the linger window; it outlives the session.
            Transient::Died => self.tabs.died(id),
            Transient::Resources { card, .. } => self.frontend_resources(id, card, bus),
            other => self.with_viewport(id, |vp| vp.transient(&other)),
        }
    }

    /// The agent's `/resources` card arrives with its own rows; the frontend
    /// appends the accumulators it owns.  Here, at the render seam, because
    /// only this thread may read the tabs and viewports.  Chrome, never
    /// recorded — no `Display` twin exists to draw it instead.
    fn frontend_resources(
        &mut self,
        id: AgentId,
        mut card: crate::bus::card::Card,
        bus: &BusReceiver,
    ) {
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
        self.push_chrome(id, RailShape::Plain, line::render_card(&card, 3));
        // The `views.dead` row is a count; this names each tombstoned
        // view's id, status, and log path.
        let tombstones = self.tabs.tombstone_lines();
        if !tombstones.is_empty() {
            self.push_chrome(id, RailShape::Plain, tombstones);
        }
    }

    /// Hand the session's viewport to `f`.
    fn with_viewport(&mut self, id: AgentId, f: impl FnOnce(&mut Viewport)) {
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
    /// recorded: unlike `Forensic::SystemNote` it never becomes a fact.
    pub(super) fn push_note(&mut self, id: AgentId, text: &str) {
        self.push_chrome(id, RailShape::Plain, line::note(text));
    }

    /// The UI-thread twin of `Avatar::note_error`, for view commands that
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
    pub(in crate::tui) fn flush_focused_log(&mut self) -> io::Result<PathBuf> {
        let focused = self.tabs.focused();
        let vp = self
            .tabs
            .viewport_mut(focused)
            .expect("focused tab always has a viewport");
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
            false,
            Inbox::new(),
            AgentRegistry::new(),
        );
        (app, rx)
    }

    fn text(app: &mut App, id: AgentId) -> String {
        let w = app
            .tabs
            .viewport_mut(id)
            .expect("the tab under test has a viewport")
            .render_window(READ_W, 40);
        w.lines.iter().map(plain).collect::<Vec<_>>().join("\n")
    }

    /// A pin is ambient register state, and the coalescing that used to be
    /// The grouping window is `SurfaceBuffer`'s, entirely worker-side: a pin
    /// never reaches that buffer at all, so it cannot split a run it is never
    /// offered to.  This drives the real production pipeline — `SurfaceBuffer`
    /// grouping into a `Display::ObservationGroup` commit, folded by
    /// `record::View`, drawn by `Viewport::sync`.
    #[test]
    fn a_pin_never_splits_a_coalesced_observation_run() {
        use crate::record::commit::SurfaceBuffer;
        use crate::record::{Blocks, Emitter as RecordEmitter, FleetSink, Fold, Printer, View};

        let (mut app, _rx) = app();
        let path = std::env::temp_dir().join(format!(
            "exarch-pin-coalesce-test-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        let recorder = RecordEmitter::create(&path).expect("temp record log");
        let (tx, brx) = crate::bus::channel();
        recorder.attach(FleetSink {
            id: 1,
            tx: tx.downgrade(),
            meter: crate::bus::UsageMeter::default(),
        });
        let _recorded = recorder
            .emit(crate::record::Display::ToolCall {
                tool: "ral".into(),
                cmd: "read 'a.rs'".into(),
                summary: Some("look around".into()),
            })
            .unwrap();

        let read_at = |path: &str| {
            Observation::instant(
                CallSite::default(),
                None,
                Observed::Read { path: path.into() },
            )
        };
        let mut buf = SurfaceBuffer::new();
        buf.absorb_observation(&recorder, 1, read_at("a.rs"))
            .unwrap();
        // The pin lands directly in the viewport's own register — it is
        // ambient state like usage, never routed through the buffer that
        // groups reads — so it cannot stand between the two below.
        app.tabs
            .viewport_mut(1)
            .expect("root has a viewport")
            .set_pin(
                "tasks".into(),
                Card(vec![Mark::Raw {
                    bytes: b"one left".to_vec(),
                }]),
            );
        buf.absorb_observation(&recorder, 1, read_at("b.rs"))
            .unwrap();
        buf.flush_surfaces(&recorder).unwrap();

        let mut blocks = Blocks::default();
        while let Ok(sig) = brx.try_recv() {
            if let crate::bus::Signal::Fact(_, rec) = sig {
                View::step(&mut blocks, &rec).expect("every commit here is a Display record");
            }
        }
        let vp = app.tabs.viewport_mut(1).expect("root has a viewport");
        vp.sync(&blocks);

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
        use crate::record::{Display, Record, Recorded, Seq, Stamp};

        let (mut app, rx) = app();
        app.transient(
            2,
            Transient::Born {
                log_dir: std::env::temp_dir(),
                name: "helper".into(),
                parent: Some(1),
            },
            &rx,
        );
        let stamp = Stamp::new(Seq::new(1), 0..0);
        app.fact(
            2,
            &Recorded::new(
                stamp,
                Record::Display(Display::Answer {
                    text: "alive".into(),
                }),
            ),
        );
        app.transient(2, Transient::Died, &rx);
        let blocks = app
            .tabs
            .viewport(2)
            .expect("the child keeps its viewport through the linger window")
            .probe_figures()
            .0;

        let stamp = Stamp::new(Seq::new(2), 0..0);
        app.fact(
            2,
            &Recorded::new(
                stamp,
                Record::Display(Display::Answer {
                    text: "straggler".into(),
                }),
            ),
        );

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
