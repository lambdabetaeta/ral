//! One [`App`] owns the tabs, scrollbacks, prompt, and gesture state, and folds
//! the [`crate::bus::Signal`] stream — `Fact` through [`App::fact`], `Born`
//! through [`App::born`], `Transient` through [`App::transient`] — into
//! scrollback blocks.

use super::block::Chrome;
use super::gesture::{Effect, GestureState, Toast};
use super::login::LoginOverlay;
use super::matrix::{self, Matrix, MatrixSort, Nav};
use super::picker::Picker;
use super::prompt::PromptState;
use super::providers::ProvidersOverlay;
use super::resources::{BusFigures, ScrollbackFigures, frontend_rows};
use super::rewind::RewindOverlay;
use super::scrollback::Scrollback;
use super::tabs::{TabRow, Tabs};
use crate::agent::fleet::Fleet;
use crate::bus::{BusReceiver, UsageMeter};
use crate::card::Card;
use crate::provider::identity::Account;
use crate::provider::{Provider, Usage};
use crate::record::{AgentId, AgentState, Born, Record, Recorded, Resumed, Segment, Transient};
use clipboard::Clipboard;

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
    Rewind(RewindOverlay),
    Providers(ProvidersOverlay),
}

/// The focused tab's committed lines flow into the host terminal's native
/// scrollback; off-focus tabs accumulate locally and replay in full on focus.
pub(crate) struct App {
    pub(super) tabs: Tabs,
    pub(super) prompt_state: PromptState,
    /// What opens every root's view, born or cleared.
    opening: Card,
    /// This run's spend, metered at the seam whatever any view shows.
    meter: UsageMeter,
    /// What the resumed sessions' histories had spent.
    recalled: Usage,
    pub(super) status_model: String,
    /// Modal in behaviour — an early-return guard in [`Self::key`] — and in
    /// rendering: drawn last, over the dimmed session.
    pub(super) overlay: Option<Overlay>,
    pub(super) gesture: GestureState,
    pub(super) clipboard: Clipboard,
    /// A render-time projection over `tabs`, never a reshuffle of the model.
    pub(super) matrix_sort: MatrixSort,
    /// The matrix's cursor, deliberately separate from [`Tabs::focus`]: moving
    /// around the matrix must not retarget the prompt until the user presses
    /// Enter.
    pub(super) matrix: Matrix,
    pub(super) cwd_basename: String,
    /// Lets `render::emit_tab_title` skip the write when the title is unchanged.
    pub(super) last_title: String,
}

impl App {
    /// Opens on `first`, the first agent born: its tab, focused, and its
    /// session's card and replay, as any later root's.
    pub fn new(
        first: AgentId,
        born: &Born,
        fleet: Arc<Fleet>,
        meter: UsageMeter,
        vi: bool,
        opening: Card,
    ) -> Self {
        let cwd_basename = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "?".into());
        let mut app = Self {
            tabs: Tabs::new(first, born, fleet, Some(opening.clone())),
            meter,
            recalled: Usage::default(),
            prompt_state: PromptState::new(vi),
            opening,
            status_model: String::new(),
            overlay: None,
            gesture: GestureState::new(),
            clipboard: Clipboard::default(),
            matrix_sort: MatrixSort::default(),
            matrix: Matrix::Watching,
            cwd_basename,
            last_title: String::new(),
        };
        app.welcome(first, born.resumed);
        app
    }

    pub fn total_usage(&self) -> Usage {
        self.recalled + self.meter.total()
    }

    /// Set the status bar label from the focused agent's provider. Call at
    /// startup and after every focus or model change.
    /// `accounts` is the set the label is drawn relative to, so two logins on
    /// one email still read apart on the status line.
    pub fn update_live_model(&mut self, p: &Provider, accounts: &[Account]) {
        let status_provider = crate::provider::identity::label(p.account(), accounts);
        // The rung in force, in the picker's own ladder label: a model the
        // catalog says takes no reasoning control reads `auto`, which is what
        // goes on the wire.
        let effort = crate::provider::effort_label(&p.tuning().effort).unwrap_or("custom");
        self.status_model = format!("{status_provider}/{} ({effort})", p.model());
    }

    /// Whether the focused tab has an agent to steer: its handle upgrades for
    /// exactly as long as the agent lives, which is exactly as long as
    /// steering means anything.
    pub(super) fn is_steerable(&self) -> bool {
        self.tabs.focused_agent().is_some()
    }

    /// A dead or lingering tab has no mailbox to be busy on, so it reads as
    /// waiting.
    pub(super) fn focused_waiting(&self) -> bool {
        self.tabs
            .focused_agent()
            .is_none_or(|agent| agent.mailbox.waiting_for_input())
    }

    /// Whether matrix navigation owns the keyboard.
    pub(super) fn matrix_navigating(&self) -> bool {
        matches!(self.matrix, Matrix::Navigating(_))
    }

    /// The cursor this frame draws: a cursor whose agent has gone reads as the
    /// attached tab, the way a stale focus reads.  Resolved at every
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

    /// Mutable access to the active `/model` picker, for `model_picker`'s `drive`.
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

    /// Mutable access to the active `/providers` overlay, for `providers::drive`.
    pub(super) fn providers_mut(&mut self) -> Option<&mut ProvidersOverlay> {
        match self.overlay.as_mut() {
            Some(Overlay::Providers(p)) => Some(p),
            _ => None,
        }
    }

    /// Mutable access to the active `/rewind` overlay, for `rewind::drive`.
    pub(super) fn rewind_mut(&mut self) -> Option<&mut RewindOverlay> {
        match self.overlay.as_mut() {
            Some(Overlay::Rewind(r)) => Some(r),
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

    /// Fold one witnessed record fact into the screen — the sole way a
    /// `Display`/`Forensic` commit reaches it — through the scrollback that
    /// mirrors its log ([`Scrollback::fact`]).  A fact that starts the mirror
    /// over is a `/clear`, which retires the agent's descendants' tabs.  A
    /// tab in the linger window is frozen: it still renders its final frame
    /// and ages out, but no further event belongs in it.
    pub fn fact(&mut self, id: AgentId, segment: Segment, rec: &Recorded<Record>) {
        if self.tabs.lingering(id) {
            return;
        }
        if self.with_scrollback(id, |sb| sb.fact(segment, rec)) == Some(true) {
            self.tabs.retire_subtree(id);
            self.gesture.clear_selection();
        }
    }

    /// Open the newborn's tab — a root's under the session card — and mirror
    /// its log as it stands, a resumed session's under the note that marks
    /// where its history ends.
    pub fn born(&mut self, id: AgentId, born: &Born) {
        let opening = born.parent.is_none().then(|| self.opening.clone());
        self.tabs.born(id, born, opening);
        self.welcome(id, born.resumed);
    }

    fn welcome(&mut self, id: AgentId, resumed: Option<Resumed>) {
        let Some(Resumed { turn, bytes }) = resumed else {
            self.with_scrollback(id, Scrollback::catch_up);
            return;
        };
        if let Some(spent) = self.with_scrollback(id, |sb| {
            sb.recall(bytes);
            sb.usage()
        }) {
            self.recalled += spent;
        }
        self.push_note(
            id,
            &format!("resumed: {turn} turns, {} KB", bytes.div_ceil(1024)),
        );
        self.with_scrollback(id, Scrollback::catch_up);
    }

    /// Draw one live-only transient directly, with no log-backed fold: the
    /// mirror of [`Self::fact`] for [`crate::bus::Signal::Transient`].
    /// [`Transient::Died`]/[`Transient::Resources`] need the tabs a bare
    /// `Scrollback` cannot see, so they are answered here; everything else
    /// forwards to [`Scrollback::transient`] on the recording scrollback.
    pub fn transient(&mut self, id: AgentId, t: Transient, bus: &BusReceiver) {
        if self.tabs.lingering(id) {
            return;
        }
        match t {
            Transient::Died => self.tabs.died(id),
            Transient::Resources { card, .. } => self.frontend_resources(id, card, bus),
            Transient::Limits { card } => self.push_chrome(id, Chrome::Framed(card)),
            other => {
                self.with_scrollback(id, |sb| sb.transient(&other));
            }
        }
    }

    /// The agent's `/resources` card arrives with its own rows; the frontend
    /// appends the accumulators it owns.  Here, at the render seam, because
    /// only this thread may read the tabs and scrollbacks.  Chrome, never
    /// recorded — no `Display` twin exists to draw it instead.
    fn frontend_resources(&mut self, id: AgentId, mut card: crate::card::Card, bus: &BusReceiver) {
        let (blocks, rows, bytes) = self
            .tabs
            .scrollback(id)
            .map_or((0, 0, 0), super::scrollback::Scrollback::probe_figures);
        let frontend = frontend_rows(
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
        card.0.push(crate::card::Mark::heading("frontend"));
        card.0.push(crate::agent::resources::rows_mark(&frontend));
        self.push_chrome(id, Chrome::Framed(card));
    }

    /// Hand the session's scrollback to `f`.
    fn with_scrollback<R>(&mut self, id: AgentId, f: impl FnOnce(&mut Scrollback) -> R) -> Option<R> {
        let Some(sb) = self.tabs.scrollback_mut(id) else {
            ral_core::dbg_trace!(
                "tui",
                "scrollback event DROPPED: no scrollback for id={id}; known={:?}",
                self.tabs.ids()
            );
            return None;
        };
        Some(f(sb))
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
        // An overlay is exclusive; its own keys are handled by its driver
        // (`model_picker`, `login`, `rewind`). This guard only stops a stray
        // key leaking through.
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
            KeyCode::Up if can_edit && k.modifiers.is_empty() => {
                if self.prompt_state.row() == 0 {
                    let recalled = self
                        .tabs
                        .focused_agent()
                        .is_some_and(|agent| self.prompt_state.edit_queued_prompt(&agent.mailbox));
                    if !recalled {
                        self.prompt_state.history_prev();
                    }
                } else {
                    self.prompt_state.edit_input(k);
                }
            }
            KeyCode::Down if can_edit && k.modifiers.is_empty() => {
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
        self.gesture
            .update_hover(me, self.tabs.focused_scrollback());
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
                let sent = self.clipboard.copy(&text, true).ok();
                self.gesture
                    .note_copy(sent, Toast::Copied(text.chars().count()));
            }
        }
    }

    /// Flush every scrollback — live, dying, or aged-out — to its session's
    /// `user.log`. Returns the paths in id order, stable across runs.
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
}

#[cfg(test)]
mod tests;
