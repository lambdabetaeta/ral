//! One [`App`] owns the tabs, scrollbacks, prompt, and gesture state, and folds
//! the [`crate::bus::Signal`] stream — [`Signal::Fact`] through [`Self::fact`],
//! [`Signal::Transient`] through [`Self::transient`] — into scrollback blocks.

use super::banner;
use super::block::{AgentSlot, Chrome};
use super::gesture::{Effect, GestureState};
use super::login::LoginOverlay;
use super::matrix::{self, Matrix, MatrixSort, Nav};
use super::palette::AGENT_HUES;
use super::picker::Picker;
use super::prompt::PromptState;
use super::render::draw;
use super::scrollback::Scrollback;
use super::tabs::{TabRow, Tabs};
use super::terminal::{Term, osc52_copy};
use crate::agent::Agent;
use crate::agent::resources::{BusFigures, ScrollbackFigures};
use crate::bus::{AgentId, AgentState, BusReceiver, Inbox};
use crate::provider::identity::Account;
use crate::provider::{Provider, Usage};
use crate::record::{Display, Forensic, Record, Recorded, Transient};

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::{
    io::{self},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

/// Rows one wheel notch, or one edge-drag step, scrolls; paging keys instead
/// move a frame height, measured per-keystroke off the last drawn content.
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
    /// The matrix's cursor, deliberately separate from [`Tabs::focus`]: moving
    /// around the matrix must not retarget the prompt until the user presses
    /// Enter.
    pub(super) matrix: Matrix,
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
    pub fn new(root: &Arc<Agent>, vi: bool, append_log: bool, inbox: Inbox) -> Self {
        let tabs = Tabs::new(root, append_log);
        let cwd_basename = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".into());
        Self {
            tabs,
            prompt_state: PromptState::new(vi),
            inbox,
            total_usage: Usage::default(),
            last_input: 0,
            context_window: None,
            status_model: String::new(),
            overlay: None,
            gesture: GestureState::new(),
            matrix_sort: MatrixSort::default(),
            matrix: Matrix::Watching,
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
    }

    /// Whether the focused tab has an agent to steer.  Root's own handle
    /// upgrades for exactly as long as the trunk attends, which is exactly as
    /// long as steering means anything.
    pub(super) fn is_steerable(&self) -> bool {
        self.tabs.focused_agent().is_some()
    }

    /// A dead or lingering tab has no mailbox to be busy on, so it reads as
    /// waiting.
    pub(super) fn focused_waiting(&self) -> bool {
        self.tabs
            .focused_agent()
            .is_none_or(|agent| agent.mailbox().waiting_for_input())
    }

    /// Whether matrix navigation owns the keyboard.
    pub(super) fn matrix_navigating(&self) -> bool {
        matches!(self.matrix, Matrix::Navigating(_))
    }

    /// The cursor this frame draws: a cursor whose agent has gone reads as the
    /// attached tab, the way a stale focus reads as root.  Resolved at every
    /// use, so a dying child never strands the selection off-list and nothing
    /// has to reconcile it.
    pub(super) fn matrix_cursor(&self, rows: &[TabRow<'_>]) -> Option<AgentId> {
        let Matrix::Navigating(cursor) = self.matrix else {
            return None;
        };
        Some(if rows.iter().any(|row| row.id == cursor) {
            cursor
        } else {
            self.tabs.focused()
        })
    }

    /// Apply one gesture.  Entering needs more than one row; staying does not,
    /// so a fleet that shrinks under the cursor leaves a one-row matrix the
    /// user exits, never a mode that re-arms when a child is born.
    fn matrix_nav(&mut self, nav: Nav) {
        let rows = self.tabs.rows();
        let (next, attach) = match (nav, self.matrix_cursor(&rows)) {
            (Nav::Toggle, None) if self.tabs.len() > 1 => {
                (Matrix::Navigating(self.tabs.focused()), None)
            }
            (Nav::Toggle | Nav::Leave, Some(_)) => (Matrix::Watching, None),
            (Nav::Up | Nav::Down, Some(cursor)) => (
                Matrix::Navigating(matrix::neighbour(
                    &rows,
                    self.matrix_sort,
                    cursor,
                    nav == Nav::Down,
                )),
                None,
            ),
            // Enter attaches and leaves, so attach-and-type is one gesture.
            (Nav::Attach, Some(cursor)) => (Matrix::Watching, Some(cursor)),
            _ => (self.matrix, None),
        };
        drop(rows);
        self.matrix = next;
        if let Some(id) = attach {
            self.tabs.set_focus(id);
        }
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
        if let Some(sb) = self.tabs.scrollback_mut(focused) {
            sb.set_state(AgentState::Ready);
        }
    }

    /// True while a time-driven visual must keep repainting with no event to
    /// drive it: the elapsed-wait bar, the tab-title spinner, or a copy toast —
    /// which needs one draw past its own expiry to erase itself, hence `margin`.
    pub(super) fn animating(&self, margin: Duration) -> bool {
        let pending = self
            .tabs
            .focused_scrollback()
            .is_some_and(|sb| sb.state().state.pending());
        pending || self.gesture.toast_live(margin) || !self.focused_waiting()
    }

    /// Age out sub-session tabs, reset root scrollback, zero cost, redraw the
    /// banner. The workers `/clear` cancels fade out through the usual
    /// `dying`/`LINGER` path, so their scrollbacks still reach `flush_logs`.
    pub fn clear(&mut self, info: &banner::SessionInfo<'_>, term: &mut Term) -> io::Result<()> {
        let root = self.tabs.root();
        // A tab already dying keeps its earlier death instant, so a child that
        // died just before the clear is not given a fresh full window.
        self.tabs.retire_all();
        // `route_submit` cancels the in-flight response, but the unbounded bus
        // still holds whatever the worker emitted before the streaming select
        // noticed the flag — one `wait_for_cancel` poll, ~50 ms.
        self.root_clear_drain = true;
        if let Some(sb) = self.tabs.scrollback_mut(root) {
            sb.reset();
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
        if self.tabs.lingering(id) {
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
    /// `Display`/`Forensic` commit reaches it.  The recording scrollback steps
    /// its own fold-memo and draws what the step reports
    /// ([`Scrollback::fact`]);
    /// [`Display::SubagentDone`] always lands in root's scrollback, whatever
    /// nesting depth drained the result, since the trunk is the permanent
    /// record of delegated work.
    pub fn fact(&mut self, id: AgentId, rec: &Recorded<Record>) {
        if !self.admits(
            id,
            matches!(rec.value(), Record::Display(Display::Prompt { .. })),
        ) {
            return;
        }
        // The fleet-wide running sum is a different quantity from a session's
        // own — it must survive a tab's retirement, where `Scrollback::usage`
        // reads off the view fold it is stepped alongside below.
        if let Record::Forensic(Forensic::UsageDelta { usage }) = rec.value() {
            let u = Usage::from(usage);
            if id == self.tabs.root() {
                self.last_input = u.input;
            }
            self.total_usage += u;
        }
        let target = match rec.value() {
            Record::Display(Display::SubagentDone { .. }) => self.tabs.root(),
            _ => id,
        };
        self.with_scrollback(target, |sb| sb.fact(rec));
    }

    /// Draw one live-only transient directly, with no log-backed fold: the
    /// mirror of [`Self::fact`] for [`crate::bus::Signal::Transient`].
    /// [`Transient::Born`]/[`Died`]/[`Resources`] need the tabs a bare
    /// `Scrollback` cannot see, so they are answered here; everything else
    /// forwards to [`Scrollback::transient`] on the recording scrollback.
    pub fn transient(&mut self, id: AgentId, t: Transient, bus: &BusReceiver) {
        // A `Cleared` answering *our* `/clear` is the gate's key and nothing
        // more: [`Self::clear`] already blanked the scrollback and redrew the
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
                agent,
                log_dir,
                name,
                parent,
            } => {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "modulus by AGENT_HUES.len() yields 0..6, fits u8"
                )]
                let agent_slot = AgentSlot((self.tabs.len() % AGENT_HUES.len()) as u8);
                self.tabs
                    .born(id, agent, &log_dir, name, parent, agent_slot);
            }
            // Root never enters the linger window; it outlives the session.
            Transient::Died => self.tabs.died(id),
            Transient::Resources { card, .. } => self.frontend_resources(id, card, bus),
            other => self.with_scrollback(id, |sb| sb.transient(&other)),
        }
    }

    /// The agent's `/resources` card arrives with its own rows; the frontend
    /// appends the accumulators it owns.  Here, at the render seam, because
    /// only this thread may read the tabs and scrollbacks.  Chrome, never
    /// recorded — no `Display` twin exists to draw it instead.
    fn frontend_resources(
        &mut self,
        id: AgentId,
        mut card: crate::bus::card::Card,
        bus: &BusReceiver,
    ) {
        let (blocks, rows, bytes) = self
            .tabs
            .scrollback(id)
            .map_or((0, 0, 0), super::scrollback::Scrollback::probe_figures);
        let frontend = crate::agent::resources::frontend_rows(
            ScrollbackFigures {
                blocks,
                rows,
                bytes,
                window: crate::record::BLOCKS_WINDOW as u64,
            },
            self.tabs.census(),
            BusFigures {
                depth: bus.depth() as u64,
                bytes: bus.bytes() as u64,
            },
        );
        card.0
            .push(crate::agent::resources::section_mark("frontend"));
        card.0.push(crate::agent::resources::rows_mark(&frontend));
        self.push_chrome(id, Chrome::Framed(card));
    }

    /// Hand the session's scrollback to `f`.
    fn with_scrollback(&mut self, id: AgentId, f: impl FnOnce(&mut Scrollback)) {
        match self.tabs.scrollback_mut(id) {
            Some(sb) => f(sb),
            None => {
                ral_core::dbg_trace!(
                    "tui",
                    "scrollback event DROPPED — no scrollback for id={id}; known={:?}",
                    self.tabs.ids()
                );
            }
        }
    }

    pub(super) fn push_chrome(&mut self, id: AgentId, chrome: Chrome) {
        self.with_scrollback(id, |sb| sb.push_chrome(chrome));
    }

    /// A dim view-local note — a slash legend, a clipboard ack. Drawn, not
    /// recorded: unlike `Forensic::SystemNote` it never becomes a fact.
    pub(super) fn push_note(&mut self, id: AgentId, text: &str) {
        self.push_chrome(id, Chrome::Note(text.to_owned()));
    }

    /// The UI-thread twin of `Avatar::note_error`, for view commands that
    /// surface their own failures. Drawn, not recorded.
    pub(super) fn push_error(&mut self, id: AgentId, message: &str) {
        self.push_chrome(id, Chrome::Error(message.to_owned()));
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
        // Matrix navigation is a modal keyboard surface of its own; Tab is
        // global, so it enters and leaves even with an editor prefix armed.
        if let Some(nav) = matrix::nav(&k, self.matrix_navigating()) {
            self.matrix_nav(nav);
            return;
        }
        if self.matrix_navigating() {
            // Paging still reads the transcript under the matrix; every other
            // key is swallowed so it cannot reach the draft.
            match k.code {
                KeyCode::PageUp => self.apply(self.gesture.scroll_page(-1)),
                KeyCode::PageDown => self.apply(self.gesture.scroll_page(1)),
                _ => {}
            }
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
        // Every key reaches the textarea only on an editable tab, so a lingering
        // subagent is watch-only and the global prompt stays pristine until
        // the user attaches to a live row.
        match k.code {
            // Paging scrolls any tab; bare Up/Down stay bound to history below.
            KeyCode::PageUp => self.apply(self.gesture.scroll_page(-1)),
            KeyCode::PageDown => self.apply(self.gesture.scroll_page(1)),
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
        // Motion and press alike, so the dial glyph brightens the instant the
        // pointer crosses a dialable block.
        self.gesture.update_hover(me, self.tabs.focused_scrollback());
        let effect = match me.kind {
            MouseEventKind::ScrollUp => Some(Effect::Scroll(-SCROLL_STEP)),
            MouseEventKind::ScrollDown => Some(Effect::Scroll(SCROLL_STEP)),
            MouseEventKind::Down(MouseButton::Left)
                if !me.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.gesture.press(me);
                None
            }
            MouseEventKind::Drag(MouseButton::Left) => self.gesture.drag(me, SCROLL_STEP),
            MouseEventKind::Up(MouseButton::Left) => {
                self.gesture.release(self.tabs.focused_scrollback())
            }
            _ => None,
        };
        if let Some(effect) = effect {
            self.apply(effect);
        }
    }

    /// Run a gesture's requested mutation against the focused scrollback.
    fn apply(&mut self, effect: Effect) {
        let f = self.tabs.focused();
        match effect {
            Effect::Scroll(delta) => {
                if let Some(sb) = self.tabs.scrollback_mut(f) {
                    sb.scroll_by(delta);
                }
            }
            Effect::CycleBlock(hit) => {
                if let Some(sb) = self.tabs.scrollback_mut(f) {
                    let _ = sb.cycle_block(hit);
                }
            }
            Effect::Copy(text) => {
                let outcome = osc52_copy(&text).map(|()| text.chars().count());
                self.gesture.note_copy(outcome);
            }
        }
    }

    /// Flush every scrollback — live, dying, or aged-out — to its session's
    /// `user.log`. Returns the paths root first, then subagents in dispatch
    /// order, stable across runs.
    pub fn flush_logs(&mut self) -> io::Result<Vec<PathBuf>> {
        self.tabs
            .views_mut()
            .map(|sb| Ok(sb.flush_log()?.to_path_buf()))
            .collect()
    }

    /// The focused tab's latest reply as raw markdown, for `/copy`. Empty when
    /// the tab has no scrollback or its last block is not prose.
    pub(in crate::tui) fn latest_reply(&self) -> String {
        self.tabs
            .focused_scrollback()
            .map(Scrollback::latest_reply_md)
            .unwrap_or_default()
    }

    /// Flush the focused tab's `user.log` and return its path for `/export`.
    pub(in crate::tui) fn flush_focused_log(&mut self) -> io::Result<PathBuf> {
        let focused = self.tabs.focused();
        let sb = self
            .tabs
            .scrollback_mut(focused)
            .expect("focused tab always has a scrollback");
        Ok(sb.flush_log()?.to_path_buf())
    }

    pub fn banner(&mut self, term: &mut Term, s: &banner::SessionInfo<'_>) -> io::Result<()> {
        if let Some(sb) = self.tabs.scrollback_mut(self.tabs.root()) {
            sb.push_chrome(Chrome::Splash);
            sb.push_chrome(Chrome::Session(banner::session_card(s)));
        }
        draw(self, term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::testkit::{TestAgentSpec, test_agent};
    use crate::bus::card::{Card, Mark};
    use crate::fleet::Fleet;
    use crate::tui::palette::READ_W;
    use crate::tui::row::Row;
    use ral_core::types::{CallSite, Observation, Observed};

    /// The trunk is returned alongside its `App` because the frontend holds it
    /// only weakly: dropping it here would settle the agent mid-test.
    fn app() -> (App, BusReceiver, Arc<Agent>) {
        let (_tx, rx) = crate::bus::channel();
        let fleet = Fleet::new();
        let root = test_agent(&fleet, TestAgentSpec::new("main")).expect("a fresh trunk");
        let app = App::new(&root, false, false, Inbox::new());
        (app, rx, root)
    }

    fn text(app: &mut App, id: AgentId) -> String {
        let w = app
            .tabs
            .scrollback_mut(id)
            .expect("the tab under test has a scrollback")
            .render_window(READ_W, 40);
        w.lines
            .iter()
            .map(Row::plain)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn matrix_cursor_does_not_change_focus_until_enter() {
        let (mut app, rx, root) = app();
        let child = root.id + 1;
        app.transient(
            child,
            Transient::Born {
                agent: std::sync::Weak::new(),
                log_dir: std::env::temp_dir(),
                name: "child".into(),
                parent: Some(root.id),
            },
            &rx,
        );

        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.key(key(KeyCode::Tab));
        assert!(app.matrix_navigating(), "Tab enters the matrix");
        assert_eq!(
            app.matrix_cursor(&app.tabs.rows()),
            Some(root.id),
            "the cursor starts on the attached agent"
        );

        app.key(key(KeyCode::Down));
        assert_eq!(
            app.matrix_cursor(&app.tabs.rows()),
            Some(child),
            "arrows move the matrix cursor"
        );
        assert_eq!(
            app.tabs.focused(),
            root.id,
            "moving the cursor does not retarget the prompt"
        );

        app.key(key(KeyCode::Enter));
        assert_eq!(app.tabs.focused(), child, "Enter attaches to the cursor");
        assert!(!app.matrix_navigating(), "and leaves the matrix");
    }

    #[test]
    fn esc_leaves_the_matrix_without_moving_focus() {
        let (mut app, rx, root) = app();
        app.transient(
            root.id + 1,
            Transient::Born {
                agent: std::sync::Weak::new(),
                log_dir: std::env::temp_dir(),
                name: "child".into(),
                parent: Some(root.id),
            },
            &rx,
        );

        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        app.key(key(KeyCode::Tab));
        app.key(key(KeyCode::Down));
        app.key(key(KeyCode::Esc));

        assert!(!app.matrix_navigating(), "Esc leaves the surface");
        assert_eq!(
            app.tabs.focused(),
            root.id,
            "an abandoned cursor attaches to nothing"
        );
    }

    #[test]
    fn matrix_mode_swallows_prompt_editing() {
        let (mut app, rx, root) = app();
        app.transient(
            root.id + 1,
            Transient::Born {
                agent: std::sync::Weak::new(),
                log_dir: std::env::temp_dir(),
                name: "child".into(),
                parent: Some(root.id),
            },
            &rx,
        );
        app.prompt_state.set_prompt("draft");
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(
            app.prompt_state.prompt_text(),
            "draft",
            "matrix keys cannot edit the prompt"
        );
    }

    /// A pin is ambient register state: it lands in the scrollback's own
    /// register, never in the mirror, so it cannot split the run it is offered
    /// to no block of.  The call and its two reads are one group either side
    /// of it.
    #[test]
    fn a_pin_never_splits_a_coalesced_observation_run() {
        use crate::bus::card::observation_wire;
        use crate::record::{Display, Locus, Record, Recorded, Seq};

        let (mut app, rx, root) = app();
        let mut seq = 0;
        let mut fact = |app: &mut App, display| {
            seq += 1;
            app.fact(
                root.id,
                &Recorded::new(Locus::placeholder(Seq::new(seq)), Record::Display(display)),
            );
        };
        let read_at = |path: &str| {
            observation_wire(&Observation::instant(
                CallSite::default(),
                None,
                Observed::Read { path: path.into() },
            ))
        };

        fact(
            &mut app,
            Display::ToolCall {
                tool: "ral".into(),
                cmd: "read 'a.rs'".into(),
                summary: Some("look around".into()),
            },
        );
        fact(
            &mut app,
            Display::Observation {
                value: read_at("a.rs"),
            },
        );
        app.transient(
            root.id,
            Transient::Pin {
                key: "tasks".into(),
                card: Card(vec![Mark::Raw {
                    bytes: b"one left".to_vec(),
                }]),
            },
            &rx,
        );
        fact(
            &mut app,
            Display::Observation {
                value: read_at("b.rs"),
            },
        );

        let sb = app.tabs.scrollback(root.id).expect("root has a scrollback");
        assert_eq!(
            sb.probe_figures().0,
            1,
            "the call and the two reads it produced are one block"
        );
        assert_eq!(
            sb.pins()
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            ["tasks"],
            "the pin lands in the register, never in scrollback"
        );
        let all = text(&mut app, root.id);
        assert!(
            all.contains("a.rs") && all.contains("b.rs"),
            "both reads render in the one block: {all:?}"
        );
    }

    /// A tab in the linger window has rendered its final frame: a straggler
    /// from a worker whose cancel it outran must not paint into it.
    #[test]
    fn a_dying_tab_admits_no_straggler() {
        use crate::record::{Display, Locus, Record, Recorded, Seq};

        let (mut app, rx, root) = app();
        let helper = root.id + 1;
        app.transient(
            helper,
            Transient::Born {
                agent: std::sync::Weak::new(),
                log_dir: std::env::temp_dir(),
                name: "helper".into(),
                parent: Some(root.id),
            },
            &rx,
        );
        let locus = Locus::placeholder(Seq::new(1));
        app.fact(
            helper,
            &Recorded::new(
                locus,
                Record::Display(Display::Answer {
                    text: "alive".into(),
                }),
            ),
        );
        app.transient(helper, Transient::Died, &rx);
        let blocks = app
            .tabs
            .scrollback(helper)
            .expect("the child keeps its scrollback through the linger window")
            .probe_figures()
            .0;

        let locus = Locus::placeholder(Seq::new(2));
        app.fact(
            helper,
            &Recorded::new(
                locus,
                Record::Display(Display::Answer {
                    text: "straggler".into(),
                }),
            ),
        );

        let all = text(&mut app, helper);
        assert!(all.contains("alive"), "the final frame survives: {all:?}");
        assert!(
            !all.contains("straggler"),
            "a dying tab admits no post-mortem text: {all:?}"
        );
        assert_eq!(
            app.tabs
                .scrollback(helper)
                .expect("scrollback")
                .probe_figures()
                .0,
            blocks,
            "and gains no block from the events it dropped"
        );
    }
}
