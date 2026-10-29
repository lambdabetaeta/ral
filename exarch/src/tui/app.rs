//! One [`App`] owns the tabs, scrollbacks, prompt, and gesture state, and folds
//! the [`crate::bus::Signal`] stream — `Commit` through [`App::commit`], `Born`
//! through [`App::born`], `Transient` through [`App::transient`] — into
//! scrollback blocks.

use super::block::Chrome;
use super::commands;
use super::gesture::{Effect, GestureState, Toast};
use super::login::{self, LoginOverlay};
use super::matrix::{self, Matrix, Nav};
use super::model_picker::{self, PickerSession};
use super::prompt::{Grip, PromptState};
use super::providers::{self, ProvidersOverlay};
use super::resources::{BusFigures, ScrollbackFigures, frontend_rows};
use super::rewind::{self, RewindOverlay};
use super::scrollback::Scrollback;
use super::tabs::{Status, TabRow, Tabs};
use crate::SessionInfo;
use crate::agent::AgentHandle;
use crate::agent::fleet::Fleet;
use crate::bus::{BusReceiver, Read, Rewrite, UsageMeter};
use crate::card::Card;
use crate::provider::{Bureau, Usage};
use crate::record::{AgentId, Born, Commit, Resumed, Transient};
use clipboard::Clipboard;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

/// Rows one wheel notch, or one edge-drag step, scrolls; paging keys instead
/// move a frame height, measured per-keystroke off the last drawn content.
const SCROLL_STEP: isize = 3;

/// The handles a submitted line is serviced against, bundled so the command
/// path — `route_submit` into `pick_model` / `login` — threads one context.
/// Every agent-side reach is the tabs' own, so no fleet handle rides here.
pub struct CommandCtx<'a> {
    pub(super) bureau: &'a Bureau,
    pub(super) info: &'a SessionInfo<'a>,
}

/// Work finished off the UI thread, applied on it.
pub(super) type Job = Box<dyn FnOnce(&mut App, &CommandCtx<'_>) + Send>;

/// An overlay's input, its chords already resolved.
pub(super) enum Input {
    Key(KeyCode),
    /// Ctrl-C, Ctrl-D, or Esc: every overlay's one cancel chord.
    Cancel,
    /// Ctrl-S: the save chord of an overlay that holds a form.
    Save,
    /// Bracketed paste; only an overlay with a text field reads it.
    Paste(String),
}

/// Ctrl-C, Ctrl-D, Esc → `Cancel`; Ctrl-S → `Save`; anything else its code.
pub(super) fn input_of(k: &KeyEvent) -> Input {
    if ctrl_key(k, 'c') || ctrl_key(k, 'd') || k.code == KeyCode::Esc {
        return Input::Cancel;
    }
    if ctrl_key(k, 's') {
        return Input::Save;
    }
    Input::Key(k.code)
}

/// Whether `k` is `c` pressed with the Control modifier.
pub fn ctrl_key(k: &KeyEvent, c: char) -> bool {
    matches!(k.code, KeyCode::Char(key) if key.eq_ignore_ascii_case(&c))
        && k.modifiers.contains(KeyModifiers::CONTROL)
}

fn copy_key(k: &KeyEvent) -> bool {
    match k.code {
        KeyCode::Char(c) if c.eq_ignore_ascii_case(&'c') => [
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ]
        .contains(&k.modifiers),
        KeyCode::Insert => k.modifiers == KeyModifiers::CONTROL,
        _ => false,
    }
}

#[cfg(test)]
type CopySink = Box<dyn FnMut(&str) -> std::io::Result<clipboard::Copied>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyAction {
    Edit,
    Submit,
    /// Esc — cancels, but an open overlay may claim it first to dismiss itself.
    Cancel,
    /// Ctrl-C — the interrupt; inside an overlay it closes the overlay instead.
    Interrupt,
}

/// Classify one key press in the running UI loop: Ctrl-C interrupts, Esc
/// cancels, a bare Enter submits when the focused tab is steerable, everything
/// else edits.  The modal overlays bypass this and read their chord from
/// [`input_of`].
pub(super) fn key_action(k: &KeyEvent, enter_submits: bool) -> KeyAction {
    if ctrl_key(k, 'c') {
        return KeyAction::Interrupt;
    }
    if k.code == KeyCode::Esc {
        return KeyAction::Cancel;
    }
    if enter_submits && k.code == KeyCode::Enter && k.modifiers.is_empty() {
        KeyAction::Submit
    } else {
        KeyAction::Edit
    }
}

/// The one modal overlay that may be open at a time.
pub(super) enum Overlay {
    Picker(PickerSession, AgentId),
    Login(LoginOverlay),
    Rewind(RewindOverlay),
    Providers(ProvidersOverlay),
}

/// Who a left press hands the gesture to, until release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseOwner {
    Prompt(Grip),
    Transcript,
    Terminal,
}

/// A chord prefix the next key completes or drops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prefix {
    /// `C-x`, of `C-x C-e`.
    CtrlX,
}

/// The frontend's whole state: every tab, the prompt, gestures and the open overlay.
pub(crate) struct App {
    pub(super) tabs: Tabs,
    pub(super) prompt_state: PromptState,
    /// What opens every root's view, born or cleared.
    opening: Card,
    /// This run's spend, metered at the seam whatever any view shows.
    meter: UsageMeter,
    /// What the resumed sessions' histories had spent.
    recalled: Usage,
    /// Modal in behaviour — [`Self::key`] hands it every key — and in
    /// rendering: drawn last, over the dimmed session.
    pub(super) overlay: Option<Overlay>,
    /// Counts overlays opened, so a [`Job`] can tell whether its overlay is still up.
    pub(super) overlay_gen: u64,
    /// Where off-thread work sends the [`Job`] that applies its result.
    pub(super) jobs: mpsc::Sender<Job>,
    pub(super) gesture: GestureState,
    mouse_owner: Option<MouseOwner>,
    /// Disarmed by any other input, so it never outlives the next key.
    prefix: Option<Prefix>,
    input_focus: AgentId,
    pub(super) clipboard: Clipboard,
    #[cfg(test)]
    copy_sink: Option<CopySink>,
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
    pub(super) fn new(
        first: AgentId,
        born: &Born,
        fleet: Arc<Fleet>,
        meter: UsageMeter,
        prompt: PromptState,
        opening: Card,
        jobs: mpsc::Sender<Job>,
    ) -> Self {
        // The launch directory's name for the tab title, read where no shell
        // speaks for it.
        let cwd_basename = ral_core::host::cwd()
            .and_then(|p| p.as_path().file_name().map(ral_core::path::shown))
            .unwrap_or_else(|| "?".into());
        let mut app = Self {
            tabs: Tabs::new(first, born, fleet, Some(opening.clone())),
            meter,
            recalled: Usage::default(),
            prompt_state: prompt,
            opening,
            overlay: None,
            overlay_gen: 0,
            jobs,
            gesture: GestureState::new(),
            mouse_owner: None,
            prefix: None,
            input_focus: first,
            clipboard: Clipboard::new(ral_core::host::var("SSH_CONNECTION").is_some()),
            #[cfg(test)]
            copy_sink: None,
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
            .is_none_or(|agent| agent.waiting_for_input())
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
                Matrix::Navigating(matrix::neighbour(&rows, cursor, nav == Nav::Down)),
                None,
            ),
            // Enter attaches and leaves, so attach-and-type is one gesture.
            (Nav::Attach, Some(cursor)) => (Matrix::Watching, Some(cursor)),
            _ => (self.matrix, None),
        };
        drop(rows);
        if matches!(self.matrix, Matrix::Navigating(_)) != matches!(next, Matrix::Navigating(_)) {
            self.suspend_input();
        }
        self.matrix = next;
        if let Some(id) = attach {
            self.tabs.set_focus(id);
        }
    }

    /// Open `overlay` in place of any other.
    pub(super) fn open(&mut self, overlay: Overlay) {
        self.suspend_input();
        self.overlay = Some(overlay);
        self.overlay_gen += 1;
    }

    /// Hand `input` to the open overlay; it stays open unless its handler closes it.
    pub(super) fn overlay_input(&mut self, input: &Input, ctx: &CommandCtx<'_>) {
        let Some(mut overlay) = self.overlay.take() else {
            return;
        };
        let keep = match &mut overlay {
            Overlay::Picker(s, tab) => model_picker::input(s, *tab, input, self, ctx),
            Overlay::Login(l) => login::input(l, input, self, ctx),
            Overlay::Rewind(r) => rewind::input(r, input, self),
            Overlay::Providers(p) => providers::input(p, input, self, ctx),
        };
        if keep {
            self.overlay = Some(overlay);
        }
    }

    /// True while a time-driven visual must keep repainting with no event to
    /// drive it: the elapsed-wait bar, the tab-title spinner, a copy toast —
    /// which needs one draw past its own expiry to erase itself, hence `margin` —
    /// or an open overlay, which repaints for jobs and the picker's polling.
    pub(super) fn animating(&self, margin: Duration) -> bool {
        let pending = self
            .tabs
            .focused_status()
            .is_some_and(|status| status.state.pending());
        pending
            || self.gesture.toast_live(margin)
            || !self.focused_waiting()
            || self.overlay.is_some()
    }

    /// Fold one whole commit into the screen through the scrollback that
    /// mirrors its log ([`Scrollback::commit`]). A commit that starts the mirror
    /// over is a `/clear`, which retires the agent's descendants' tabs.  A
    /// tab in the linger window is frozen: it still renders its final frame
    /// and ages out, but no further event belongs in it.
    pub fn commit(&mut self, id: AgentId, commit: &Commit) {
        if self.tabs.lingering(id) {
            return;
        }
        if self.with_scrollback(id, |sb| sb.commit(commit)) == Some(true) {
            if let Some(status) = self.tabs.status_mut(id) {
                *status = Status::new();
            }
            self.tabs.retire_subtree(id);
            self.gesture.clear();
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
    /// mirror of [`Self::commit`] for [`crate::bus::Signal::Transient`].
    /// Every variant is routed here: to the tabs, the tab's [`Status`], or its
    /// scrollback.
    pub fn transient(&mut self, id: AgentId, t: Transient, bus: &BusReceiver) {
        if self.tabs.lingering(id) {
            return;
        }
        match t {
            Transient::Died => self.tabs.died(id),
            Transient::Resources { card, .. } => self.frontend_resources(id, card, bus),
            Transient::State(state) => {
                if let Some(status) = self.tabs.status_mut(id) {
                    status.set_state(state);
                }
            }
            Transient::Pin { key, card } => {
                if let Some(status) = self.tabs.status_mut(id) {
                    status.set_pin(key, card);
                }
            }
            Transient::Unpin { key } => {
                if let Some(status) = self.tabs.status_mut(id) {
                    status.drop_pin(&key);
                }
            }
            Transient::Token(text) => {
                if let Some(status) = self.tabs.status_mut(id) {
                    status.note_streamed(text.chars().count());
                }
                self.with_scrollback(id, |sb| sb.push_token(&text));
            }
            Transient::Thinking(text) => {
                if let Some(status) = self.tabs.status_mut(id) {
                    status.note_streamed(text.chars().count());
                }
                self.with_scrollback(id, |sb| sb.push_thinking(&text));
            }
            Transient::StopReason(raw) => self.push_chrome(id, Chrome::StopReason(raw)),
            Transient::Fault { text } => self.push_chrome(id, Chrome::Error(text)),
            Transient::Boundary => {
                self.with_scrollback(id, Scrollback::close_lanes);
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
    fn with_scrollback<R>(
        &mut self,
        id: AgentId,
        f: impl FnOnce(&mut Scrollback) -> R,
    ) -> Option<R> {
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

    /// The UI-thread twin of `Agent::note_error`, for view commands that
    /// surface their own failures. Drawn, not recorded.
    pub(super) fn push_error(&mut self, id: AgentId, message: &str) {
        self.push_chrome(id, Chrome::Error(message.to_owned()));
    }

    /// Run `f` on the focused tab's agent, or say it has ended.
    pub(super) fn session(&mut self, f: impl FnOnce(&AgentHandle)) {
        if let Some(agent) = self.tabs.focused_agent() {
            f(&agent);
        } else {
            let focused = self.tabs.focused();
            self.push_error(focused, "this tab's agent has ended");
        }
    }

    pub(super) fn post_read(&mut self, read: Read) {
        self.session(|agent| agent.post_read(read));
    }

    pub(super) fn post_rewrite(&mut self, rewrite: Rewrite) {
        self.session(|agent| agent.post_rewrite(rewrite));
    }

    /// Route one key press — the loop drops releases — to its one owner, in
    /// order: overlay, selection copy, matrix, search, popup, chords, draft.
    /// `true` when the user asked to compose the draft in `$EDITOR`, which
    /// only the UI loop, owning the terminal, can do.
    pub fn key(&mut self, k: KeyEvent, ctx: &CommandCtx<'_>) -> bool {
        self.reconcile_input_focus();
        self.release_prompt_grip();
        let prefix = self.prefix.take();
        if self.overlay.is_some() {
            self.overlay_input(&input_of(&k), ctx);
            return false;
        }
        if copy_key(&k) && self.copy_prompt_selection() {
            return false;
        }
        let can_edit = self.is_steerable();
        let action = key_action(&k, can_edit);
        if action != KeyAction::Interrupt && self.matrix_navigating() {
            self.matrix_key(&k);
            return false;
        }
        if can_edit && self.prompt_state.search_key(k) {
            return false;
        }
        if action != KeyAction::Interrupt
            && (can_edit && self.prompt_state.menu_key(k) || self.matrix_key(&k))
        {
            return false;
        }
        if can_edit && action == KeyAction::Edit {
            match prefix {
                Some(Prefix::CtrlX) => return ctrl_key(&k, 'e'),
                None if ctrl_key(&k, 'g') => return true,
                None if ctrl_key(&k, 'x') => {
                    self.prefix = Some(Prefix::CtrlX);
                    return false;
                }
                None => {}
            }
        }
        match action {
            // Interrupt only the exchange; helpers remain under the work.
            KeyAction::Interrupt | KeyAction::Cancel => {
                if let Some(agent) = self.tabs.focused_agent() {
                    agent.interrupt();
                }
                // Escape also leaves Vi Insert; Ctrl-C never edits the draft.
                if action == KeyAction::Cancel && can_edit {
                    self.prompt_state.edit_input(k);
                }
            }
            KeyAction::Submit => {
                if let Some(text) = self.prompt_state.submit() {
                    // Every tab funnels through the one path, which parses
                    // before it forks on the focused tab — so no slash line can
                    // be mailed to the model.
                    commands::route_submit(text, self, ctx);
                }
            }
            KeyAction::Edit => self.edit(k),
        }
        false
    }

    /// Matrix navigation is modal: while it owns the keyboard every key but the
    /// interrupt is a matrix gesture or is swallowed, so Esc leaves the surface
    /// instead of cancelling the focused turn, and nothing reaches the draft.
    /// Tab is global, so it enters and leaves even with an editor prefix armed.
    fn matrix_key(&mut self, k: &KeyEvent) -> bool {
        if let Some(nav) = matrix::nav(k, self.matrix_navigating()) {
            self.matrix_nav(nav);
            return true;
        }
        if !self.matrix_navigating() {
            return false;
        }
        // Paging still reads the transcript under the matrix.
        match k.code {
            KeyCode::PageUp => self.apply(self.gesture.scroll_page(-1)),
            KeyCode::PageDown => self.apply(self.gesture.scroll_page(1)),
            _ => {}
        }
        true
    }

    /// Edit the draft after modal owners and global chords have declined it.
    fn edit(&mut self, k: KeyEvent) {
        let can_edit = self.is_steerable();
        // Every key reaches the textarea only on an editable tab, so a lingering
        // subagent is watch-only and the global prompt stays pristine until
        // the user attaches to a live row.
        match k.code {
            // Paging scrolls any tab; bare Up/Down stay bound to history below.
            KeyCode::PageUp => self.apply(self.gesture.scroll_page(-1)),
            KeyCode::PageDown => self.apply(self.gesture.scroll_page(1)),
            // Up/Down walk history only at the buffer's start/end; elsewhere they
            // move the cursor. Up on an empty prompt dequeues the queued run.
            KeyCode::Up if can_edit && k.modifiers.is_empty() => {
                let recalled = self
                    .tabs
                    .focused_agent()
                    .is_some_and(|agent| self.prompt_state.edit_queued_prompt(|| agent.recall()));
                if !recalled && !self.prompt_state.history_prev() {
                    self.prompt_state.edit_input(k);
                }
            }
            KeyCode::Down if can_edit && k.modifiers.is_empty() => {
                if !self.prompt_state.history_next() {
                    self.prompt_state.edit_input(k);
                }
            }
            _ if can_edit => {
                self.prompt_state.edit_input(k);
            }
            _ => {}
        }
    }

    /// Pasted text goes to the open overlay, else the draft — never under a
    /// matrix cursor, a modal surface its bytes must not leak past.
    pub fn paste(&mut self, s: &str, ctx: &CommandCtx<'_>) {
        self.reconcile_input_focus();
        self.release_prompt_grip();
        self.prefix = None;
        if self.overlay.is_some() {
            self.overlay_input(&Input::Paste(s.to_owned()), ctx);
        } else if self.is_steerable() && !self.matrix_navigating() {
            self.prompt_state.paste(s);
        }
    }

    /// Whether a key now would reach a text field, so a burst of them may be
    /// a paste: never past a matrix cursor, a watch-only tab or a chord prefix.
    pub(super) fn takes_text(&self) -> bool {
        self.overlay.is_some()
            || (self.is_steerable() && !self.matrix_navigating() && self.prefix.is_none())
    }

    fn cancel_mouse(&mut self) {
        self.mouse_owner = None;
        self.gesture.clear();
    }

    /// Typing or pasting may rewrite the draft a prompt grip's anchor indexes.
    fn release_prompt_grip(&mut self) {
        if matches!(self.mouse_owner, Some(MouseOwner::Prompt(_))) {
            self.mouse_owner = None;
        }
    }

    pub(super) fn suspend_input(&mut self) {
        self.cancel_mouse();
        self.prefix = None;
    }

    /// Reconcile before drawing or routing, including focus changes from jobs.
    pub(super) fn reconcile_input_focus(&mut self) {
        if self.input_focus != self.tabs.focused() {
            self.input_focus = self.tabs.focused();
            self.suspend_input();
        }
        if self.overlay.is_some()
            || (matches!(self.mouse_owner, Some(MouseOwner::Prompt(_)))
                && (!self.is_steerable() || self.matrix_navigating()))
        {
            self.cancel_mouse();
        }
    }

    /// A left press chooses the owner until release, independent of geometry.
    pub fn mouse(&mut self, me: MouseEvent) {
        self.reconcile_input_focus();
        self.prefix = None;
        if !self.gesture.drawn_for(self.tabs.focused()) {
            self.cancel_mouse();
            return;
        }
        if self.overlay.is_some() {
            return;
        }
        let prompt_live = self.is_steerable() && !self.matrix_navigating();
        if me.modifiers.contains(KeyModifiers::SHIFT) {
            self.cancel_mouse();
            if me.kind == MouseEventKind::Down(MouseButton::Left) {
                self.mouse_owner = Some(MouseOwner::Terminal);
            }
            return;
        }
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.cancel_mouse();
                let grip = if prompt_live {
                    self.prompt_state.press(me)
                } else {
                    None
                };
                let owner = if let Some(grip) = grip {
                    MouseOwner::Prompt(grip)
                } else {
                    self.gesture
                        .update_hover(me, self.tabs.focused_scrollback());
                    self.gesture.press(me);
                    MouseOwner::Transcript
                };
                self.mouse_owner = Some(owner);
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                let release = me.kind == MouseEventKind::Up(MouseButton::Left);
                let effect = match self.mouse_owner {
                    Some(MouseOwner::Prompt(grip)) => {
                        self.prompt_state.drag(grip, me);
                        None
                    }
                    Some(MouseOwner::Transcript) if release => {
                        self.gesture.release(self.tabs.focused_scrollback())
                    }
                    Some(MouseOwner::Transcript) => self.gesture.drag(me, SCROLL_STEP),
                    Some(MouseOwner::Terminal) | None => None,
                };
                if release {
                    self.mouse_owner = None;
                }
                if let Some(effect) = effect {
                    self.apply(effect);
                }
            }
            _ => {
                if me.kind == MouseEventKind::Down(MouseButton::Right)
                    && self.mouse_owner.is_none()
                    && self.prompt_state.contains_position(me.column, me.row)
                    && self.copy_prompt_selection()
                {
                    return;
                }
                if self.mouse_owner == Some(MouseOwner::Terminal)
                    || (self.mouse_owner != Some(MouseOwner::Transcript)
                        && prompt_live
                        && self.prompt_state.wheel(me))
                {
                    return;
                }
                self.gesture
                    .update_hover(me, self.tabs.focused_scrollback());
                let effect = match me.kind {
                    MouseEventKind::ScrollUp => Some(Effect::Scroll(-SCROLL_STEP)),
                    MouseEventKind::ScrollDown => Some(Effect::Scroll(SCROLL_STEP)),
                    _ => None,
                };
                if let Some(effect) = effect {
                    self.apply(effect);
                }
            }
        }
    }

    fn copy_text(&mut self, text: &str) -> std::io::Result<clipboard::Copied> {
        #[cfg(test)]
        if let Some(sink) = &mut self.copy_sink {
            return sink(text);
        }
        self.clipboard.copy(text, true)
    }

    /// A failed write consumes the chord, not the selection.  Only an editable,
    /// un-navigated prompt copies, so key and click cannot drift apart.
    fn copy_prompt_selection(&mut self) -> bool {
        if !self.is_steerable() || self.matrix_navigating() {
            return false;
        }
        let Some(text) = self.prompt_state.selection_text() else {
            return false;
        };
        let sent = match self.copy_text(&text) {
            Ok(sent) => {
                self.prompt_state.clear_selection();
                Some(sent)
            }
            Err(error) => {
                self.push_error(
                    self.tabs.focused(),
                    &format!("could not copy prompt selection; retry copying: {error}"),
                );
                None
            }
        };
        self.gesture
            .note_copy(sent, Toast::Copied(text.chars().count()));
        true
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
                let sent = match self.copy_text(&text) {
                    Ok(sent) => Some(sent),
                    Err(error) => {
                        self.push_error(f, &format!("could not copy selection: {error}"));
                        None
                    }
                };
                self.gesture
                    .note_copy(sent, Toast::Copied(text.chars().count()));
            }
        }
    }

    /// The focused tab's latest reply as raw markdown, for `/copy`. Empty when
    /// the tab has no scrollback or its last block is not prose.
    pub(in crate::tui) fn latest_reply(&self) -> String {
        self.tabs
            .focused_scrollback()
            .map(Scrollback::latest_reply_md)
            .unwrap_or_default()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
pub(super) mod tests;
