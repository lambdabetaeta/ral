//! Full-screen TUI frontend.
//!
//! One [`Sink`] implementation, plus the REPL loop the user types into.
//! The TUI owns raw-mode, bracketed-paste, the alternate screen, and
//! mouse capture through [`TerminalGuard`]; the agent core in
//! [`crate::bus`] and [`crate::session`] sees only an
//! [`crate::bus::Emitter`] / [`Event`] channel.
//!
//! The app owns its scrollback rather than delegating it to the host
//! terminal: each session is a buffer of collapsible [`block`]s and the
//! whole frame is redrawn every tick.  A tool call shows its summary and
//! opens to the full ral script on a click; the wheel scrolls, click-drag
//! selects and copies, and Shift-drag falls through to the terminal's own
//! selection.  Assistant text accumulates into the active [`Viewport`]'s
//! paragraph buffer and commits one fence-safe paragraph at a time — no
//! live preview row.

mod block;
mod line;
mod md;
mod picker;
mod viewport;

use line::usage_text;

use crate::bootstrap::Scratch;
use crate::bus::{Event, Hunk, Kind, PromptQueue, SessionId, Sink, pump};
use crate::cancel;
use crate::credential::CredentialStore;
use crate::models::{LiveSource, ModelCatalog, ModelSource};
use crate::provider::{self, Provider, Usage};
use crate::session::Session;
use crate::state;

use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
    },
};
use picker::Picker;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::event::{
        Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind, poll as ct_poll, read as ct_read,
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState},
};
use ratatui_textarea::TextArea;
use std::{
    collections::HashMap,
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError},
    time::{Duration, Instant},
};

use line::{
    BANNER_CYAN, BANNER_GOLD, BANNER_LIME, BANNER_ORANGE, BANNER_PINK, BANNER_PURPLE, BANNER_RED,
    CYAN, LIME, ORANGE, PINK, PURPLE, READ_W, SLATE, bold, slate, slate_owned,
};
use viewport::Viewport;

const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Vaporwave trio swept across the spinner: every third frame advances
/// the colour, so the dot rotates ~1.1s per full braille cycle and the
/// colour completes its pink → purple → cyan loop every ~1s.
const SPIN_C: [Color; 3] = [PINK, PURPLE, CYAN];
const SPIN_T: u128 = 110;
pub(super) const PROMPT_PAD_H: u16 = 1;
const ART: &str = include_str!("../data/banner.txt");
const EAGLE: &str = include_str!("../data/eagle.txt");
/// How long a subagent tab stays in the rotation after the session
/// dies — long enough for the user to tab over and inspect the final
/// frame of its scrollback, short enough not to clutter the tab bar.
const LINGER: Duration = Duration::from_secs(60);
/// Display label for the root session in the tab bar.
const ROOT_TITLE: &str = "main";

pub type Term = Terminal<CrosstermBackend<Stdout>>;

/// Rows a wheel notch moves the view; paging keys move a frame-height at
/// a time, derived per-keystroke from the last drawn content height.
const SCROLL_STEP: usize = 3;

/// Raw-byte ceiling for an OSC 52 yank: base64-expanded (3→4 bytes) this
/// stays under the tightest common per-sequence cap (kitty's 8 KiB), so
/// the terminal accepts rather than silently drops the sequence.
const YANK_CAP: usize = 6000;

/// RAII guard for the raw-mode + bracketed-paste + alternate-screen +
/// mouse-capture lifetime.  Cleanup is in `Drop` so it can't be skipped
/// on unwind.
///
/// While the guard is live, file descriptor 2 is redirected to a
/// per-process log file so that `dbg_trace!` (and any other stray
/// `eprintln!`) does not tear through the rendered frame.  The
/// original fd is restored in `Drop`, after the TUI is torn down,
/// so post-session writes land on the user's real shell.
pub struct TerminalGuard {
    term: Term,
    #[cfg(unix)]
    stderr_backup: Option<std::os::fd::RawFd>,
}

impl TerminalGuard {
    #[cfg_attr(not(unix), allow(unused_variables))]
    pub fn enter(stderr_log: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        let stderr_backup = Some(redirect_stderr_to_file(stderr_log)?);
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture
        )?;
        let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        term.hide_cursor()?;
        Ok(Self {
            term,
            #[cfg(unix)]
            stderr_backup,
        })
    }

    pub fn term(&mut self) -> &mut Term {
        &mut self.term
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.term.show_cursor();
        // Leaving the alternate screen restores the user's primary buffer
        // intact, so there is nothing to clear; just unwind the modes in
        // reverse.
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
        // Restore stderr last — while teardown is running, any stray
        // diagnostics still belong in the log file, not on the user's
        // freshly-restored prompt row.
        #[cfg(unix)]
        if let Some(backup) = self.stderr_backup.take() {
            restore_stderr(backup);
        }
    }
}

/// Open `path` for append and alias it onto fd 2, returning a `dup` of
/// the original fd 2 so the caller can restore it later.  `dbg_trace!`
/// writes to fd 2 directly via `eprintln!`; without this redirect those
/// writes interleave with the rendered frame and corrupt it.  Child
/// processes that inherit fds (re-execed sandbox helpers)
/// pick up the redirected fd, so their `dbg_trace!` output flows into
/// the same log.
#[cfg(unix)]
fn redirect_stderr_to_file(path: &Path) -> io::Result<std::os::fd::RawFd> {
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    let file = OpenOptions::new().create(true).append(true).open(path)?;
    // SAFETY: STDERR_FILENO is always a valid kernel fd in a normal
    // process; `dup` either returns a new fd or `-1` with `errno` set.
    let backup = unsafe { libc::dup(libc::STDERR_FILENO) };
    if backup < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `file.as_raw_fd()` is open for the duration of this
    // block; `dup2` atomically closes the existing fd 2 and aliases
    // the source onto it.  After `dup2`, the kernel holds an
    // independent fd-table entry for fd 2 backed by the same open
    // file description, so dropping `file` here is fine.
    let r = unsafe { libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO) };
    if r < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: `backup` is a valid fd we just obtained from `dup`.
        unsafe {
            libc::close(backup);
        }
        return Err(e);
    }
    Ok(backup)
}

/// Restore fd 2 from the `dup` saved by [`redirect_stderr_to_file`].
/// Best-effort: any failure inside the TUI's drop has nowhere useful
/// to surface, and the process is about to return to the user's
/// shell anyway.
#[cfg(unix)]
fn restore_stderr(backup: std::os::fd::RawFd) {
    // SAFETY: `backup` is a live fd returned by `dup`; `dup2` is
    // idempotent on the target and `close` releases the backup.
    unsafe {
        libc::dup2(backup, libc::STDERR_FILENO);
        libc::close(backup);
    }
}

/// The main TUI application state.
///
/// Owns one [`Viewport`] per session and a flat list of visible tabs.
/// The currently focused tab's committed lines flow into the host
/// terminal's native scrollback; off-focus tabs accumulate locally
/// and replay in full when the user tabs to them.
pub struct App {
    /// Per-session scrollback.  Populated by `Born`, retained across
    /// `Died` and across tab-bar expiry so [`Self::flush_logs`] can
    /// still write each session's `user.log` at session end.
    viewports: HashMap<SessionId, Viewport>,
    /// Insertion order of viewports — root first, then subagents as
    /// they were born.  Drives [`Self::flush_logs`] for stable
    /// per-session log paths across runs.
    dispatch_order: Vec<SessionId>,
    /// Tabs visible in the tab bar.  Always starts with `root`; sub-
    /// agents are appended on `Born` and removed when their entry in
    /// `dying` ages out past [`LINGER`].
    tabs: Vec<SessionId>,
    /// Per-session label.  Root maps to [`ROOT_TITLE`]; subagents to
    /// the `title` field of their `Kind::Born` event.
    titles: HashMap<SessionId, String>,
    /// Death timestamps for subagents in their linger window.  Tabs
    /// drop from [`Self::tabs`] once [`LINGER`] elapses; the viewport
    /// stays alive for log flushing.
    dying: HashMap<SessionId, Instant>,
    root: SessionId,
    /// Tab the user is currently viewing.  Reads route through
    /// [`Self::focused`] so a stale id (expired tab) resolves to root.
    focus: SessionId,
    pub textarea: TextArea<'static>,
    /// Submitted prompts, oldest first.  Up from the prompt's first
    /// row and Down from its last row walk this list shell-style.
    history: Vec<String>,
    /// Position within [`Self::history`] while browsing, or `None`
    /// when the prompt holds the live draft rather than a recalled
    /// entry.  `Some(i)` means the prompt currently shows `history[i]`.
    hist_pos: Option<usize>,
    /// The in-progress line stashed when history browsing begins,
    /// restored when Down walks back past the newest entry.
    draft: String,
    /// Prompts the user submitted while a turn was in flight, oldest first. The
    /// queue is shared with the worker for this frontend: `Session::dispatch`
    /// may drain a non-slash prefix after a tool result to steer the next
    /// assistant step; the REPL still drains any remainder at the next turn
    /// boundary ([`Self::take_queue`]). Until then, pending messages render in
    /// the strip above the input ([`line::queued_prompt`]) and bare Up on an
    /// empty prompt pulls the newest one back for editing.
    queue: PromptQueue,
    busy_since: Option<Instant>,
    /// The worker's current phase label ([`Kind::Phase`]), shown beside
    /// the spinner so a silent local op reads "typechecking…" rather than
    /// a bare dot.  Set by a `Phase` event, cleared by any other event and
    /// when the turn ends.
    phase: Option<String>,
    total_usage: Usage,
    /// Last turn's prompt size (genai's `prompt_tokens`, which already
    /// folds the cache-read and cache-creation counts in); drives the
    /// `ctx N%` gauge.  Overwritten, not accumulated.
    last_input: u64,
    /// Hidden when `None` (native providers with no fetched catalog).
    context_window: Option<u64>,
    /// The live `provider model` shown in the per-frame status bar,
    /// updated on a `/model` switch. The startup banner is one-shot
    /// chrome; this is where the current model stays visible.
    status_model: String,
    /// The active `/model` picker, taking over the prompt region while
    /// open. `None` when the prompt is the normal text editor. Modal in
    /// behaviour (an early-return guard in [`Self::key`]), flat in
    /// rendering — a strip, not a floating overlay.
    picker: Option<Picker>,
    /// In-flight aggregation of consecutive `Kind::Patch` events
    /// targeting the same `(session, path)`.  Each `edit` invocation
    /// emits its own `Kind::Patch` carrying one [`Hunk`]; without
    /// grouping, ten consecutive edits to one file would render as ten
    /// separate `❖ patch <path>` blocks rather than one block with ten
    /// located hunks, the way a unified diff presents a single file.
    /// The buffer accumulates hunks until any non-Patch chrome lands
    /// (next tool call, assistant text, step boundary, session death),
    /// at which point [`Self::flush_patch_buf`] emits one block.
    patch_buf: Option<PatchBuf>,
    /// Geometry of the content area as of the last [`Self::draw`], so a
    /// mouse event arriving between frames maps to a buffer row.
    frame: Option<FrameGeom>,
    /// Active linewise drag-selection in focused-viewport row
    /// coordinates, painted reversed and copied on release.
    selection: Option<(usize, usize)>,
    /// In-flight left-button gesture: the row pressed, the block under
    /// it, and whether the pointer has since moved (a drag, not a click).
    press: Option<Press>,
}

/// Where the content area sat in the last drawn frame.
#[derive(Clone, Copy)]
struct FrameGeom {
    text: Rect,
    /// First visible buffer row, mapping screen rows to buffer rows.
    offset: usize,
}

/// A left-button press in progress.
struct Press {
    /// Buffer row under the press — the selection anchor.
    row: usize,
    /// Block under the press, toggled on a click that never dragged.
    block: Option<usize>,
    dragged: bool,
}

/// Accumulator backing [`App::patch_buf`].
struct PatchBuf {
    id: SessionId,
    path: String,
    hunks: Vec<Hunk>,
}

impl App {
    pub fn new(root_id: SessionId, root_log_dir: &Path, context_window: Option<u64>) -> Self {
        let mut viewports = HashMap::new();
        viewports.insert(root_id, Viewport::new(root_log_dir.join("user.log")));
        let mut titles = HashMap::new();
        titles.insert(root_id, ROOT_TITLE.to_string());
        let mut textarea = TextArea::default();
        textarea.set_wrap_mode(ratatui_textarea::WrapMode::WordOrGlyph);
        textarea.set_style(Style::default().fg(Color::White));
        textarea.set_cursor_line_style(Style::default().fg(Color::White));
        textarea.set_block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(Style::default().fg(PINK))
                .padding(ratatui::widgets::Padding::horizontal(PROMPT_PAD_H)),
        );
        Self {
            viewports,
            dispatch_order: vec![root_id],
            tabs: vec![root_id],
            titles,
            dying: HashMap::new(),
            root: root_id,
            focus: root_id,
            textarea,
            history: Vec::new(),
            hist_pos: None,
            draft: String::new(),
            queue: PromptQueue::new(),
            busy_since: None,
            phase: None,
            total_usage: Usage::default(),
            patch_buf: None,
            last_input: 0,
            context_window,
            status_model: String::new(),
            picker: None,
            frame: None,
            selection: None,
            press: None,
        }
    }

    pub fn total_usage(&self) -> Usage {
        self.total_usage
    }

    /// Set the live `provider model` label shown in the status bar. Set
    /// once at startup and again on every `/model` switch.
    pub fn set_status_model(&mut self, provider: &str, model: &str) {
        self.status_model = format!("{provider} {model}");
    }

    /// Mutable access to the active picker, for the REPL's picker loop.
    fn picker_mut(&mut self) -> Option<&mut Picker> {
        self.picker.as_mut()
    }

    pub fn busy_on(&mut self) {
        self.busy_since = Some(Instant::now());
    }
    pub fn busy_off(&mut self) {
        self.busy_since = None;
        self.phase = None;
    }

    /// Drop sub-session viewports, reset root scrollback, zero cost,
    /// redraw the banner.
    pub fn clear(&mut self, info: &SessionInfo<'_>, term: &mut Term) -> io::Result<()> {
        let root = self.root;
        self.viewports.retain(|&k, _| k == root);
        self.dispatch_order = vec![root];
        self.tabs = vec![root];
        self.titles.retain(|&k, _| k == root);
        self.dying.clear();
        self.focus = root;
        if let Some(vp) = self.viewports.get_mut(&root) {
            vp.reset();
        }
        self.total_usage = Usage::default();
        self.last_input = 0;
        self.patch_buf = None;
        self.selection = None;
        self.press = None;
        self.banner(term, info)
    }

    /// Route one event to its viewport.  Born registers a pane; Died
    /// flushes; Usage accumulates globally; everything else renders to
    /// one viewport via [`line`](mod@line).
    pub fn handle(&mut self, Event { id, kind }: Event) {
        // A phase label names the silent gap before the next thing
        // happens, so any other event supersedes it.
        if !matches!(kind, Kind::Phase(_)) {
            self.phase = None;
        }
        match kind {
            Kind::Born { log_dir, title } => {
                if let std::collections::hash_map::Entry::Vacant(slot) = self.viewports.entry(id) {
                    slot.insert(Viewport::new(log_dir.join("user.log")));
                    self.dispatch_order.push(id);
                }
                self.titles.insert(id, title);
                if !self.tabs.contains(&id) {
                    self.tabs.push(id);
                }
            }
            Kind::Died => {
                self.flush_patch_buf();
                if let Some(vp) = self.viewports.get_mut(&id) {
                    vp.flush_open();
                }
                // Root never enters the linger window; it lives as
                // long as the program does.
                if id != self.root {
                    self.dying.insert(id, Instant::now());
                }
            }
            Kind::Usage(u) => {
                // `u.input` (genai's `prompt_tokens`) already folds in the
                // cache_creation and cache_read counts on every adapter, so
                // adding them again double-counts the prompt — ~2x on a
                // cache-heavy session, on the one gauge that tells the user
                // when to `/compact` (X4).  Take the prompt total as-is.
                self.last_input = u.input;
                self.total_usage += u;
            }
            Kind::Token(text) => {
                self.flush_patch_buf();
                if let Some(vp) = self.viewports.get_mut(&id) {
                    vp.push_token(&text);
                }
            }
            Kind::Boundary => {
                self.flush_patch_buf();
                if let Some(vp) = self.viewports.get_mut(&id) {
                    vp.close_boundary();
                }
            }
            Kind::Step(n) => self.push_chrome(id, line::step(n as usize)),
            Kind::Phase(label) => self.phase = Some(label),
            Kind::ToolCall { tool, cmd, summary } => {
                ral_core::dbg_trace!("tui", "ToolCall tool={tool} cmd={cmd:?}");
                self.with_viewport(id, |vp| match summary {
                    // A summary marks a call worth revealing: the label
                    // shows shut, the script on a click.  Summary-less
                    // calls (`fff`, invalid input) have nothing to open.
                    Some(s) => vp.push_tool_call(tool, s, cmd),
                    None => vp.push_chrome(line::tool_call_static(&cmd, tool)),
                });
            }
            // Tool results never reach the rail — the script the user can
            // open is the whole of what a call surfaces.  The model still
            // receives the full result through the history pipeline.
            Kind::ToolResult(_) => {}
            Kind::UserPromptEcho(text) => self.push_chrome(id, line::user_prompt(&text)),
            Kind::StopReason(raw) => self.push_chrome(id, line::stop_reason(&raw)),
            Kind::Error(msg) => self.push_chrome(id, line::error(&msg)),
            Kind::Dim(text) => self.push_chrome(id, line::dim(&text)),
            Kind::ProviderError(error) => self.push_chrome(id, line::provider_error(&error)),
            Kind::SubagentDone {
                title,
                text,
                error,
                elapsed,
            } => {
                let lines = subagent_breadcrumb(&title, &text, error.as_deref(), elapsed);
                // Always lands in root, regardless of which nesting
                // level emitted — main is the permanent record of
                // delegated work.
                let root = self.root;
                self.push_chrome(root, lines);
            }
            // Rail-surfaced kit events.  A kit that raised one through the
            // `surface` builtin made an explicit choice to communicate
            // with the user, and patches / writes / task transitions are
            // the canonical user-visible side effects of a tool call.
            Kind::Patch { path, hunk } => {
                ral_core::dbg_trace!(
                    "tui",
                    "Patch id={id} viewports={:?} focus={} path={path}",
                    self.viewports.keys().copied().collect::<Vec<_>>(),
                    self.focus
                );
                self.absorb_patch(id, path, hunk);
            }
            Kind::Wrote {
                path,
                lines,
                preview,
            } => self.push_chrome(id, line::wrote(&path, lines, &preview)),
            Kind::Task { status, desc } => self.push_chrome(id, line::task(status, &desc)),
            Kind::Meter { done, total, label } => {
                self.push_chrome(id, line::meter(done, total, &label))
            }
        }
    }

    /// Commit any pending patch, then hand the session's viewport to `f`.
    /// Any non-Patch content closes the patch grouping window: a pending
    /// buffer must land before the new block, or the merged `❖ patch`
    /// would appear *after* whatever follows it on the rail.
    fn with_viewport(&mut self, id: SessionId, f: impl FnOnce(&mut Viewport)) {
        self.flush_patch_buf();
        match self.viewports.get_mut(&id) {
            Some(vp) => f(vp),
            None => {
                ral_core::dbg_trace!(
                    "tui",
                    "viewport event DROPPED — no viewport for id={id}; known={:?}",
                    self.viewports.keys().copied().collect::<Vec<_>>()
                );
            }
        }
    }

    fn push_chrome(&mut self, id: SessionId, lines: Vec<Line<'static>>) {
        self.with_viewport(id, |vp| vp.push_chrome(lines));
    }

    /// Absorb a `Kind::Patch`'s hunk into [`Self::patch_buf`], or flush +
    /// open a fresh buffer when the path or session changes.  Consecutive
    /// same-`(id, path)` events append their hunks into one buffer so they
    /// later render as a single `❖ patch <path>` block of located hunks —
    /// the way a unified diff presents several changes to one file.
    fn absorb_patch(&mut self, id: SessionId, path: String, hunk: Hunk) {
        let same = self
            .patch_buf
            .as_ref()
            .is_some_and(|b| b.id == id && b.path == path);
        if same {
            let buf = self.patch_buf.as_mut().expect("same-path implies Some");
            buf.hunks.push(hunk);
        } else {
            self.flush_patch_buf();
            self.patch_buf = Some(PatchBuf {
                id,
                path,
                hunks: vec![hunk],
            });
        }
    }

    /// Commit any pending [`PatchBuf`] as one `❖ patch` block.  Called
    /// at every commit boundary that isn't another `Kind::Patch`
    /// targeting the same `(id, path)`: `push_chrome`, the streaming
    /// token / boundary paths, session death, and `/clear`.
    fn flush_patch_buf(&mut self) {
        let Some(buf) = self.patch_buf.take() else {
            return;
        };
        if let Some(vp) = self.viewports.get_mut(&buf.id) {
            vp.push_patch(buf.path, buf.hunks);
        }
    }

    /// Redraw the whole frame: the focused session's visible rows and a
    /// scrollbar fill the content area, with a blank breathing row, then
    /// the tab bar, status row, prompt, and footer pinned beneath.  The
    /// content geometry is stashed in [`Self::frame`] so the next mouse
    /// event maps to a buffer row.
    pub fn draw(&mut self, term: &mut Term) -> io::Result<()> {
        let (cols, rows) = size().unwrap_or((READ_W, 24));
        let area = Rect::new(0, 0, cols, rows);
        // The picker takes over the prompt region and wants more rows than
        // a one-line prompt; otherwise the prompt box sizes to its draft.
        let prompt_h = match &self.picker {
            Some(p) => p.height(area.height),
            None => prompt_height(&self.textarea, area.width, area.height),
        };
        let tab_h = if self.tabs.len() > 1 { 1u16 } else { 0u16 };
        // The pending-prompt strip above the input: messages the user queued
        // mid-turn, waiting for the next tool-result or turn boundary. Its
        // width matches the content column (the scrollbar's column reserved),
        // and its height is capped at a third of the screen so a long queue can
        // never crowd the transcript off-screen.
        let queued = self.queue.snapshot();
        let queued_lines = if queued.is_empty() {
            Vec::new()
        } else {
            let w = area.width.saturating_sub(1).min(READ_W);
            line::queued_prompt(&queued, w, (area.height / 3).max(1) as usize)
        };
        let queued_h = queued_lines.len() as u16;
        let layout = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1), // breathing row between output and chrome
            Constraint::Length(tab_h),
            Constraint::Length(1),
            Constraint::Length(queued_h),
            Constraint::Length(prompt_h),
            Constraint::Length(1),
        ])
        .split(area);
        let (content, tab_row, status_row, queued_row, prompt_row, footer_row) = (
            layout[0], layout[2], layout[3], layout[4], layout[5], layout[6],
        );
        // Reserve the rightmost column of the content area for the scrollbar.
        let body = Layout::horizontal([Constraint::Min(1), Constraint::Length(1)]).split(content);
        let (text_rect, sb_rect) = (body[0], body[1]);

        let focused = self.focused();
        let (mut lines, offset, total) = match self.viewports.get_mut(&focused) {
            Some(vp) => {
                let w = vp.render_window(text_rect.width, text_rect.height as usize);
                (w.lines, w.offset, w.total)
            }
            None => (Vec::new(), 0, 0),
        };
        self.paint_selection(&mut lines, offset);
        self.frame = Some(FrameGeom {
            text: text_rect,
            offset,
        });

        self.style_prompt();
        let busy = self.busy_since;
        let phase = self.phase.clone();
        let usage = self.total_usage;
        let last_input = self.last_input;
        let context_window = self.context_window;
        let status_model = self.status_model.clone();
        let tab_line =
            (self.tabs.len() > 1).then(|| tab_bar(&self.tabs, &self.titles, focused, &self.dying));
        let prompt_hint = self.prompt_hint(focused);
        let picker = self.picker.as_ref();

        term.draw(|f| {
            f.render_widget(Paragraph::new(lines), text_rect);
            let mut sb = ScrollbarState::new(total)
                .position(offset)
                .viewport_content_length(text_rect.height as usize);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                sb_rect,
                &mut sb,
            );
            if let Some(line) = tab_line {
                f.render_widget(Paragraph::new(line), tab_row);
            }
            if !queued_lines.is_empty() {
                f.render_widget(Paragraph::new(queued_lines), queued_row);
            }
            f.render_widget(
                Paragraph::new(rule_line(
                    text_rect.width.min(READ_W) as usize,
                    busy,
                    phase.as_deref(),
                    &usage,
                    last_input,
                    context_window,
                    &status_model,
                )),
                status_row,
            );
            // The `/model` picker takes over the prompt region while open —
            // a flat strip, not a floating overlay. Input lives in main
            // only; a subagent tab shows a watch-only hint in the prompt
            // slot, and the textarea keeps its draft for when the user tabs
            // home.
            match (picker, prompt_hint) {
                (Some(p), _) => p.render(f, prompt_row),
                (None, Some(line)) => {
                    let block = ratatui::widgets::Block::default()
                        .borders(ratatui::widgets::Borders::ALL)
                        .border_type(ratatui::widgets::BorderType::Rounded)
                        .border_style(Style::default().fg(SLATE).add_modifier(Modifier::DIM))
                        .padding(ratatui::widgets::Padding::horizontal(PROMPT_PAD_H));
                    f.render_widget(Paragraph::new(line).block(block), prompt_row);
                }
                (None, None) => f.render_widget(&self.textarea, prompt_row),
            }
            f.render_widget(Paragraph::new(footer_hint()), footer_row);
        })?;
        Ok(())
    }

    /// Reverse-video the rows of the active selection that fall within
    /// the visible window.
    fn paint_selection(&self, lines: &mut [Line<'static>], offset: usize) {
        let Some((a, b)) = self.selection else {
            return;
        };
        let (lo, hi) = (a.min(b), a.max(b));
        for (i, line) in lines.iter_mut().enumerate() {
            if (lo..=hi).contains(&(offset + i)) {
                for span in &mut line.spans {
                    span.style = span.style.add_modifier(Modifier::REVERSED);
                }
            }
        }
    }

    /// The watch-only banner shown in the prompt slot on a subagent tab,
    /// or `None` on main where the textarea is editable.
    fn prompt_hint(&self, focused: SessionId) -> Option<Line<'static>> {
        if focused == self.root {
            return None;
        }
        let title = self.titles.get(&focused).map(String::as_str).unwrap_or("?");
        Some(Line::from(Span::styled(
            format!(" watching {title} — Tab back to main to type "),
            Style::default()
                .fg(SLATE)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        )))
    }

    /// Currently focused tab.  Resolves a stale focus (subagent that
    /// aged out of the tab bar) to the root.
    pub(in crate::tui) fn focused(&self) -> SessionId {
        if self.tabs.contains(&self.focus) {
            self.focus
        } else {
            self.root
        }
    }

    /// Expire `dying` entries that have outlived [`LINGER`].  Called
    /// once per frame from the event loop.  When the focused tab
    /// expires, focus snaps back to root.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let expired: Vec<SessionId> = self
            .dying
            .iter()
            .filter(|&(_, &t)| now.duration_since(t) >= LINGER)
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            self.dying.remove(&id);
            self.tabs.retain(|&t| t != id);
            self.titles.remove(&id);
            if self.focus == id {
                self.focus = self.root;
            }
        }
    }

    pub fn submit(&mut self) -> Option<String> {
        let s = self.prompt_text();
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            let prompt = t.to_string();
            // Append to history, collapsing immediate repeats, and
            // drop back to the live draft for the next prompt.
            if self.history.last() != Some(&prompt) {
                self.history.push(prompt.clone());
            }
            self.hist_pos = None;
            self.draft.clear();
            self.textarea.clear();
            Some(prompt)
        }
    }

    /// Submit the current draft onto the pending-prompt queue rather
    /// than running it now — the path taken by Enter while a turn is in
    /// flight. Returns `true` when a non-empty prompt was queued.
    pub fn enqueue(&mut self) -> bool {
        match self.submit() {
            Some(p) => {
                self.queue.push(p);
                true
            }
            None => false,
        }
    }

    /// Coalesce and take the queued prompts still waiting at the turn boundary
    /// — oldest first, joined by a blank line — or `None` when nothing is
    /// queued. Prompts drained by the worker between tool calls have already
    /// left the shared queue.
    pub fn take_queue(&mut self) -> Option<String> {
        self.queue.drain_joined()
    }

    /// Pull the newest pending prompt back into the editor for revision.
    /// A non-empty live draft wins over queue editing: Up keeps its ordinary
    /// history behaviour rather than discarding text the user has started.
    fn edit_queued_prompt(&mut self) -> bool {
        if self.hist_pos.is_some() || self.textarea.lines().iter().any(|line| !line.is_empty()) {
            return false;
        }
        let Some(prompt) = self.queue.pop_back() else {
            return false;
        };
        self.set_prompt(&prompt);
        true
    }

    pub fn paste(&mut self, s: &str) {
        self.textarea.insert_str(s);
    }

    /// The prompt's current contents, lines newline-joined.
    fn prompt_text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Recolor the prompt text in place: a line that exactly matches a known
    /// slash command (so [`Repl::handle_slash`] will dispatch it) glows cyan
    /// and bold; anything else stays plain white. Driven once per frame from
    /// [`App::draw`], so it tracks every edit — typing, paste, history recall.
    fn style_prompt(&mut self) {
        let style = if is_slash_command(&self.prompt_text()) {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        self.textarea.set_style(style);
        self.textarea.set_cursor_line_style(style);
    }

    /// Replace the prompt's contents, leaving the cursor at the end.
    fn set_prompt(&mut self, s: &str) {
        self.textarea.clear();
        self.textarea.insert_str(s);
    }

    /// Recall the previous prompt (Up from the first row).  The live
    /// draft is stashed on entry; navigation clamps at the oldest
    /// entry.  No-op when no prompts have been submitted yet.
    fn history_prev(&mut self) {
        let pos = match self.hist_pos {
            _ if self.history.is_empty() => return,
            None => {
                self.draft = self.prompt_text();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.hist_pos = Some(pos);
        let entry = self.history[pos].clone();
        self.set_prompt(&entry);
    }

    /// Recall the next prompt (Down from the last row), or restore the
    /// stashed draft once browsing walks past the newest entry.  No-op
    /// when not browsing history.
    fn history_next(&mut self) {
        let Some(i) = self.hist_pos else {
            return;
        };
        if i + 1 < self.history.len() {
            self.hist_pos = Some(i + 1);
            let entry = self.history[i + 1].clone();
            self.set_prompt(&entry);
        } else {
            self.hist_pos = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_prompt(&draft);
        }
    }

    pub fn key(&mut self, k: KeyEvent) {
        if k.kind != KeyEventKind::Press {
            return;
        }
        // The `/model` picker is modal: while it is open no key reaches the
        // textarea or the scrollback. Its own key handling runs in the
        // REPL's picker loop ([`Repl::pick_model`]), which drives the
        // picker directly; this guard only keeps a stray key (e.g. one
        // arriving on a non-prompt path) from leaking through.
        if self.picker.is_some() {
            return;
        }
        // Ctrl+Y yanks the focused tab's full committed history to the
        // system clipboard via OSC 52 — works on any tab, including
        // subagents (handy when you want to grab the run's transcript
        // before its linger window expires).  Native terminal selection
        // is still the right tool for sub-ranges; this is the
        // "grab everything" shortcut.
        if k.code == KeyCode::Char('y') && k.modifiers.contains(KeyModifiers::CONTROL) {
            if let Some(vp) = self.viewports.get(&self.focused()) {
                // OSC 52 has a per-sequence cap (see `osc52_copy`); a full
                // transcript blows past it and the terminal drops the whole
                // sequence, copying nothing.  Bound the payload to its tail —
                // the most-recent content, which is what Ctrl-Y is for.
                let _ = osc52_copy(tail_bytes(&vp.yank_text(), YANK_CAP));
            }
            return;
        }
        // Tab cycles regardless of focus; every other key is delivered
        // to the textarea *only* when main is focused.  Subagent tabs
        // are watch-only — they keep the global textarea pristine for
        // when the user tabs home.
        match k.code {
            // Paging scrolls the focused pane on any tab; bare Up/Down
            // stay bound to prompt history below.
            KeyCode::PageUp => self.scroll_page(-1),
            KeyCode::PageDown => self.scroll_page(1),
            // Not collapsible into a match guard: with <=1 tab, Tab must
            // be a no-op, not fall through to the textarea-input arm below.
            #[allow(clippy::collapsible_match)]
            KeyCode::Tab => {
                if self.tabs.len() > 1 {
                    let current = self.focused();
                    if let Some(pos) = self.tabs.iter().position(|&id| id == current) {
                        self.focus = self.tabs[(pos + 1) % self.tabs.len()];
                    }
                }
            }
            // Up/Down walk the prompt history, but only from the
            // prompt's edge rows: with the cursor mid-text in a
            // multi-line draft they fall through and move the cursor.
            // When the prompt is empty and prompts are queued above it,
            // Up first pulls the newest queued prompt back down for
            // editing, removing it from the pending queue.
            KeyCode::Up if self.focused() == self.root && k.modifiers.is_empty() => {
                if self.textarea.cursor().0 == 0 {
                    if !self.edit_queued_prompt() {
                        self.history_prev();
                    }
                } else {
                    self.textarea.input(k);
                }
            }
            KeyCode::Down if self.focused() == self.root && k.modifiers.is_empty() => {
                let last_row = self.textarea.lines().len() - 1;
                if self.textarea.cursor().0 == last_row {
                    self.history_next();
                } else {
                    self.textarea.input(k);
                }
            }
            _ if self.focused() == self.root => {
                self.textarea.input(k);
            }
            _ => {}
        }
    }

    /// Route a mouse event: the wheel scrolls, a left-drag selects (and
    /// copies on release), and a left click that never dragged opens the
    /// block it landed on.  Shift+left falls through to the terminal's
    /// own selection, so we never see — or fight — it.
    pub fn mouse(&mut self, me: MouseEvent) {
        match me.kind {
            MouseEventKind::ScrollUp => self.scroll(-(SCROLL_STEP as isize)),
            MouseEventKind::ScrollDown => self.scroll(SCROLL_STEP as isize),
            MouseEventKind::Down(MouseButton::Left)
                if !me.modifiers.contains(KeyModifiers::SHIFT) =>
            {
                self.press(me)
            }
            MouseEventKind::Drag(MouseButton::Left) => self.drag(me),
            MouseEventKind::Up(MouseButton::Left) => self.release(),
            _ => {}
        }
    }

    /// Scroll the focused pane by `delta` rows (negative = up).
    fn scroll(&mut self, delta: isize) {
        let id = self.focused();
        if let Some(vp) = self.viewports.get_mut(&id) {
            if delta < 0 {
                vp.scroll_up((-delta) as usize);
            } else {
                vp.scroll_down(delta as usize);
            }
        }
    }

    /// Scroll one content-height per page key, falling back to a sane
    /// step before the first frame is drawn.
    fn scroll_page(&mut self, dir: isize) {
        let page = self
            .frame
            .map(|f| f.text.height.saturating_sub(1).max(1) as isize)
            .unwrap_or(10);
        self.scroll(dir * page);
    }

    /// Begin a left-button gesture: drop any prior selection, anchor at
    /// the pressed row, and remember the block under it.
    fn press(&mut self, me: MouseEvent) {
        self.selection = None;
        self.press = None;
        let Some(frame) = self.frame else { return };
        if !contains(frame.text, me.column, me.row) {
            return;
        }
        let row = frame.offset + (me.row - frame.text.y) as usize;
        let id = self.focused();
        let block = self.viewports.get(&id).and_then(|vp| vp.block_at(row));
        self.press = Some(Press {
            row,
            block,
            dragged: false,
        });
    }

    /// Extend the selection to the dragged-to row, clamped to the
    /// visible window.
    fn drag(&mut self, me: MouseEvent) {
        let Some(frame) = self.frame else { return };
        let Some(press) = &mut self.press else { return };
        press.dragged = true;
        let anchor = press.row;
        let rel = me
            .row
            .saturating_sub(frame.text.y)
            .min(frame.text.height.saturating_sub(1));
        self.selection = Some((anchor, frame.offset + rel as usize));
    }

    /// Finish a left-button gesture: a drag copies its selection, a bare
    /// click opens the tool call it landed on.
    fn release(&mut self) {
        let Some(press) = self.press.take() else {
            return;
        };
        let id = self.focused();
        if press.dragged {
            if let Some((a, b)) = self.selection
                && let Some(vp) = self.viewports.get(&id)
            {
                let _ = osc52_copy(&vp.selection_text(a.min(b), a.max(b)));
            }
        } else if let Some(idx) = press.block {
            if let Some(vp) = self.viewports.get_mut(&id) {
                vp.toggle_block(idx);
            }
            self.selection = None;
        }
    }

    /// Walk every viewport (live, dying, or aged-out) and flush its
    /// rendered-text accumulator to that session's `user.log`.
    /// Returns the list of paths, root first, then subagents in
    /// dispatch order — stable across runs for testing.
    pub fn flush_logs(&mut self) -> io::Result<Vec<PathBuf>> {
        // Flush the open markdown buffer first so any trailing
        // streamed paragraph (no double-newline yet) reaches
        // `committed`, and the `user.log`, before the final flush.
        for vp in self.viewports.values_mut() {
            vp.flush_open();
        }
        let mut paths = Vec::with_capacity(self.dispatch_order.len());
        for &id in &self.dispatch_order {
            if let Some(vp) = self.viewports.get_mut(&id) {
                paths.push(vp.flush_log()?.to_path_buf());
            }
        }
        Ok(paths)
    }

    pub fn banner(&mut self, term: &mut Term, s: &SessionInfo<'_>) -> io::Result<()> {
        let (tw, _) = size().unwrap_or((READ_W, 24));
        let cap = tw.min(READ_W) as usize;
        let mut ls: Vec<Line<'static>> = vec![Line::default()];
        for (a, e) in ART.lines().zip(EAGLE.lines()) {
            ls.push(Line::from(vec![
                bold(a.to_string(), BANNER_PINK),
                Span::raw("  "),
                bold(e.to_string(), BANNER_GOLD),
            ]));
        }
        ls.push(Line::from(Span::styled(
            format!(" {}", s.cwd),
            Style::default().fg(SLATE).add_modifier(Modifier::ITALIC),
        )));
        let max_t = match (s.max_tokens_override, s.max_output_tokens) {
            (Some(n), _) => n.to_string(),
            (None, Some(catalog)) => format!("auto (≤{})", fmt_tokens(catalog as u64)),
            (None, None) => "auto".into(),
        };
        let mut line2 = vec![
            slate("provider "),
            bold(s.provider.into(), BANNER_CYAN),
            slate("    model "),
            bold(s.model.into(), BANNER_LIME),
        ];
        if let Some(slug) = s.canonical_slug
            && slug != s.model
        {
            line2.push(slate(" ("));
            line2.push(slate_owned(slug.to_string()));
            line2.push(slate(")"));
        }
        ls.push(Line::from(line2));
        ls.push(Line::from(vec![
            slate("max-tokens "),
            bold(max_t, BANNER_LIME),
        ]));
        if let Some(ctx) = s.context_window {
            ls.push(Line::from(vec![
                slate("context "),
                bold(fmt_tokens(ctx), BANNER_LIME),
            ]));
        }
        let bc = if s.base == "dangerous" {
            BANNER_RED
        } else {
            BANNER_ORANGE
        };
        let ext = s
            .extend_base
            .map(|p| bold(p.display().to_string(), BANNER_ORANGE))
            .unwrap_or(slate("none"));
        let restr = if s.restrict_files.is_empty() {
            slate("none")
        } else {
            Span::raw(
                s.restrict_files
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        };
        ls.push(Line::from(vec![
            slate("base "),
            bold(s.base.into(), bc),
            slate("    extend-base "),
            ext,
            slate("    restrict "),
            restr,
        ]));
        let sz = format!("{:.1} kB", s.system_size as f64 / 1024.0);
        ls.push(Line::from(vec![
            slate("system prompt "),
            bold(sz, BANNER_LIME),
            slate(" · "),
            if s.system_files.is_empty() {
                slate("default")
            } else {
                Span::raw(
                    s.system_files
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                )
            },
        ]));
        ls.push(Line::from(vec![
            slate("scratch "),
            bold(s.scratch.display().to_string(), BANNER_ORANGE),
        ]));
        ls.push(Line::from(Span::styled(
            "━".repeat(cap),
            Style::default().fg(BANNER_PURPLE),
        )));
        if let Some(vp) = self.viewports.get_mut(&self.root) {
            vp.push_chrome(ls);
        }
        self.draw(term)
    }
}

/// Metadata shown in the startup banner.
pub struct SessionInfo<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    /// Canonical catalog slug, shown after `model` when distinct.
    pub canonical_slug: Option<&'a str>,
    /// `None` means "auto" — genai's per-adapter default applies.
    pub max_tokens_override: Option<u32>,
    /// `None` for native providers (no fetched catalog).
    pub context_window: Option<u64>,
    /// Informational; the request-time override is what actually flies.
    pub max_output_tokens: Option<u32>,
    pub system_size: usize,
    pub system_files: &'a [PathBuf],
    pub base: &'a str,
    pub extend_base: Option<&'a Path>,
    pub restrict_files: &'a [PathBuf],
    pub scratch: &'a Path,
    pub cwd: &'a str,
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

fn rule_line(
    width: usize,
    busy_since: Option<Instant>,
    phase: Option<&str>,
    usage: &Usage,
    last_input: u64,
    context_window: Option<u64>,
    status_model: &str,
) -> Line<'static> {
    let mut spans = Vec::new();
    let mut left_w = 0usize;
    if let Some(t0) = busy_since {
        let step = (t0.elapsed().as_millis() / SPIN_T) as usize;
        let g = SPIN[step % SPIN.len()];
        let col = SPIN_C[(step / 3) % SPIN_C.len()];
        spans.push(Span::styled(
            format!("{g} "),
            Style::default().fg(col).add_modifier(Modifier::BOLD),
        ));
        left_w += 2;
        if let Some(p) = phase {
            let label = Span::styled(format!("{p}… "), Style::default().fg(SLATE));
            left_w += label.width();
            spans.push(label);
        }
    }
    if !status_model.is_empty() {
        let segment: Vec<Span<'static>> = vec![
            Span::styled(status_model.to_string(), Style::default().fg(SLATE)),
            Span::styled(" · ", Style::default().fg(SLATE)),
        ];
        left_w += segment.iter().map(|s| s.width()).sum::<usize>();
        spans.extend(segment);
    }
    if let Some(cap) = context_window
        && cap > 0
    {
        let pct = ((last_input as f64 / cap as f64) * 100.0).round() as u64;
        let pct = pct.min(999);
        let ctx_segment: Vec<Span<'static>> = vec![
            Span::styled("ctx ", Style::default().fg(SLATE)),
            Span::styled(format!("{pct}%"), Style::default().fg(SLATE)),
            Span::styled(" · ", Style::default().fg(SLATE)),
        ];
        left_w += ctx_segment.iter().map(|s| s.width()).sum::<usize>();
        spans.extend(ctx_segment);
    }
    let right = usage_text(usage);
    let rw: usize = right.iter().map(|s: &Span<'_>| s.width()).sum();
    spans.push(Span::styled(
        "─".repeat(width.saturating_sub(left_w + rw)),
        Style::default().fg(SLATE),
    ));
    spans.extend(right);
    Line::from(spans)
}

/// Build the breadcrumb that lands in root's scrollback when a
/// subagent finishes.  Layout:
///
/// ```text
/// ↘ refactor-output  [done in 47s]
/// <the subagent's final assistant message, rendered as markdown>
///                                   ← trailing blank for separation
/// ```
///
/// On failure / cancel the body is omitted and the header carries
/// `[failed: <reason>]` or `[cancelled]` instead.
fn subagent_breadcrumb(
    title: &str,
    text: &str,
    error: Option<&str>,
    elapsed: Duration,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let secs = elapsed.as_secs();
    let suffix = match error {
        None if text.is_empty() => "[done, no output]".to_string(),
        None => format!("[done in {secs}s]"),
        Some(reason) if reason.eq_ignore_ascii_case("cancelled") => "[cancelled]".to_string(),
        Some(reason) => format!("[failed: {reason}]"),
    };
    let (title_color, suffix_style) = if error.is_some() {
        (
            ORANGE,
            Style::default().fg(ORANGE).add_modifier(Modifier::DIM),
        )
    } else {
        (LIME, Style::default().fg(SLATE).add_modifier(Modifier::DIM))
    };
    lines.push(Line::from(vec![
        Span::styled("↘ ", Style::default().fg(SLATE)),
        bold(title.to_string(), title_color),
        Span::raw("  "),
        Span::styled(suffix, suffix_style),
    ]));
    if error.is_none() && !text.is_empty() {
        let (tw, _) = size().unwrap_or((READ_W, 24));
        lines.extend(md::render_md(text, tw.min(READ_W), md::MD_INDENT));
    }
    lines.push(Line::default());
    lines
}

/// Whether the cell `(col, row)` lies inside `rect`.
fn contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// One-row tab bar.  Focused tab in bold + cyan, live subagents in
/// slate, dying subagents in slate dim with a countdown.  Shown only
/// when there's more than one tab — root-only sessions skip the row
/// entirely.
fn tab_bar(
    tabs: &[SessionId],
    titles: &HashMap<SessionId, String>,
    focused: SessionId,
    dying: &HashMap<SessionId, Instant>,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(tabs.len() * 2);
    for (i, &id) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let title = titles.get(&id).map(String::as_str).unwrap_or("?");
        let label: String = if id == focused {
            format!("[{title}]")
        } else if let Some(t) = dying.get(&id) {
            let left = LINGER.saturating_sub(t.elapsed()).as_secs();
            format!(" {title} ({left}s) ")
        } else {
            format!(" {title} ")
        };
        let style = if id == focused {
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD)
        } else if dying.contains_key(&id) {
            Style::default().fg(SLATE).add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(SLATE)
        };
        spans.push(Span::styled(label, style));
    }
    Line::from(spans)
}

/// Emit `text` to the host terminal's system clipboard via OSC 52.
///
/// Uses the ST (`\e\\`) terminator rather than BEL because modern tmux
/// in passthrough mode forwards ST more reliably.  Terminals impose
/// per-sequence size limits (kitty defaults to 8 KiB; iTerm2 silently
/// drops oversized payloads); callers should bound the slice they pass
/// to something screen-sized.
///
/// For tmux users: requires `set -g set-clipboard on`, and on tmux 3.3+
/// `set -g allow-passthrough on` as well — otherwise tmux strips the
/// sequence before it reaches the host terminal.
fn osc52_copy(text: &str) -> io::Result<()> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::io::Write;
    let payload = STANDARD.encode(text);
    let mut out = io::stdout().lock();
    write!(out, "\x1b]52;c;{payload}\x1b\\")?;
    out.flush()
}

/// The last `cap` bytes of `text`, snapped forward to the nearest char
/// boundary so the slice is always valid UTF-8.  Returns all of `text`
/// when it already fits.
fn tail_bytes(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn footer_hint() -> Line<'static> {
    let st = Style::default()
        .fg(SLATE)
        .add_modifier(Modifier::DIM | Modifier::ITALIC);
    let hint = " Tab pane • click ▸ expand • drag copy (⇧ native) • wheel/PgUp scroll • Ctrl-Y yank • Ctrl-C cancel • Ctrl-D quit ";
    Line::from(Span::styled(hint, st))
}

/// Count visual lines after soft-wrapping each logical line at `width`.
///
/// This re-derives the wrap independently of the widget: `ratatui-
/// textarea` wraps with its own engine (the configured
/// [`ratatui_textarea::WrapMode::WordOrGlyph`]) and exposes no query
/// for its rendered height, so the box height is sized from this
/// parallel computation.
/// The two engines agree for the common case (ASCII, no tabs) but can
/// disagree on tab expansion, wide/CJK glyphs, multi-codepoint
/// graphemes, and the cursor sitting one past an exactly-full row.
/// An undersize is not self-correcting: the widget's viewport scrolls
/// down to chase the cursor and never scrolls back up when slack
/// opens, so one short frame would hide the head of the draft for the
/// rest of the edit.  [`prompt_height`] therefore floors the height at
/// the widget's own cursor row.  Keep `width` the widget's effective
/// text width (the rect minus its border and [`PROMPT_PAD_H`] padding)
/// and this wrap aligned with the textarea's configured
/// [`TextArea::set_wrap_mode`] so the divergence stays in that corner.
fn prompt_visual_line_count(textarea: &TextArea<'_>, width: u16) -> usize {
    textarea
        .lines()
        .iter()
        .map(|line| {
            if line.is_empty() {
                1
            } else {
                textwrap::wrap(
                    line,
                    textwrap::Options::new(width as usize).break_words(true),
                )
                .len()
                .max(1)
            }
        })
        .sum()
}

/// Compute prompt-box height: visual line count plus the two rows the
/// rounded border eats (top + bottom), clamped to ⅔ of the available
/// height with a floor of 3 (one text row + the border).  The block
/// has [`PROMPT_PAD_H`] columns of horizontal padding inside its left
/// and right borders; wrap inside that.
fn prompt_height(textarea: &TextArea<'_>, width: u16, max_h: u16) -> u16 {
    let text_w = width.saturating_sub(2 + 2 * PROMPT_PAD_H);
    // Floor at the widget's own cursor row: its viewport scrolls down
    // to chase a cursor the box is too short for and never scrolls
    // back, so the box must always have room for the cursor's row.
    let rows =
        prompt_visual_line_count(textarea, text_w).max(textarea.screen_cursor().row + 1) as u16;
    let with_border = rows.saturating_add(2);
    with_border.min((max_h * 2 / 3).max(3)).max(3).min(max_h)
}

// ── Sink + REPL ─────────────────────────────────────────────────────────

/// Pairs the terminal lifetime with the app so one `&mut Tui` serves as
/// the [`Sink`] argument to [`pump`].  Fields are accessed via direct
/// field syntax (`self.guard.term()` alongside `&mut self.app`) for
/// disjoint-borrow splitting.
pub struct Tui {
    guard: TerminalGuard,
    app: App,
}

impl Tui {
    pub fn new(
        root_id: SessionId,
        root_log_dir: &Path,
        context_window: Option<u64>,
        stderr_log: &Path,
    ) -> io::Result<Self> {
        let guard = TerminalGuard::enter(stderr_log)?;
        let app = App::new(root_id, root_log_dir, context_window);
        Ok(Self { guard, app })
    }
}

impl Sink for Tui {
    fn handle(&mut self, e: Event) {
        self.app.handle(e);
    }

    fn prompt_queue(&self) -> PromptQueue {
        self.app.queue.clone()
    }

    fn drive(&mut self, rx: Receiver<Event>) -> io::Result<()> {
        self.app.busy_on();
        let r = drive_events(self.guard.term(), &mut self.app, rx);
        self.app.busy_off();
        r
    }
}

enum Slash {
    Quit,
    Continue,
    Prompt,
}

/// Channel carrying `(provider, fetched models or failure)` from the
/// per-provider background fetch threads back to the picker loop.
type FetchRx = std::sync::mpsc::Receiver<(provider::ProviderKind, Result<Vec<String>, String>)>;

struct Repl<'a> {
    tui: Tui,
    session: &'a mut Session,
    /// The active provider, owned so a `/model` switch can rebuild it in
    /// place — a swappable field rather than a launch-time `&Provider`.
    provider: Provider,
    info: &'a SessionInfo<'a>,
    /// The auto-discovered credentials a `/model` switch draws the chosen
    /// provider's key from, and the live model catalog the picker fetches
    /// through. The picker shows every available provider's models; a
    /// switch rebuilds the transport over the same transcript.
    store: &'a CredentialStore,
    catalog: &'a mut ModelCatalog<LiveSource>,
    scratch: &'a Scratch,
}

/// Build the [`Tui`], banner, run the REPL, flush logs, print log
/// paths + usage on the restored shell.
#[allow(clippy::too_many_arguments)]
pub fn run(
    session: &mut Session,
    provider: Provider,
    info: &SessionInfo<'_>,
    store: &CredentialStore,
    catalog: &mut ModelCatalog<LiveSource>,
    scratch: &Scratch,
    run_dir: &std::path::Path,
    seed: Option<String>,
) -> Result<(), String> {
    let caps = provider::caps_for(provider.model());
    let stderr_log = run_dir.join("stderr.log");
    let mut tui = Tui::new(
        session.id,
        session.log_dir(),
        caps.context_window,
        &stderr_log,
    )
    .map_err(|e| format!("ratatui init: {e}"))?;
    let status_provider = crate::oauth::provider_label(provider.is_subscription(), info.provider);
    tui.app.set_status_model(&status_provider, info.model);
    let mut r = Repl {
        tui,
        session,
        provider,
        info,
        store,
        catalog,
        scratch,
    };
    r.tui
        .app
        .banner(r.tui.guard.term(), info)
        .map_err(|e| e.to_string())?;
    let drive = r.drive(seed);
    let logs = r
        .tui
        .app
        .flush_logs()
        .map_err(|e| format!("session logs: {e}"));
    let usage = r.tui.app.total_usage();
    // Restore the terminal before printing so log paths land on the
    // user's normal shell rather than the alt screen.
    drop(r);
    if let Ok(paths) = &logs {
        for p in paths {
            match p.parent() {
                Some(dir) => println!("Session logs: {} (user.log + events.json)", dir.display()),
                None => println!("Session log: {}", p.display()),
            }
        }
    } else if let Err(e) = logs {
        eprintln!("exarch: {e}");
    }
    println!("{usage}");
    drive
}

/// One row of the slash-command registry: the canonical token, any aliases,
/// a one-line description for `/help`, and the handler that runs it.
struct SlashCommand {
    name: &'static str,
    aliases: &'static [&'static str],
    help: &'static str,
    run: fn(&mut Repl<'_>) -> Slash,
}

/// The slash-command registry — the single source of truth. Dispatch
/// ([`Repl::handle_slash`]), the prompt-box highlight ([`is_slash_command`]),
/// and the `/help` listing all read from here, so they cannot drift.
///
/// Each `run` wraps its method in a non-capturing closure rather than naming
/// the method path directly: a method item never generalizes the `Repl`
/// lifetime into the fn-pointer's binder, so `Repl::cmd_quit as fn(_)` is
/// `for<'a> fn(&'a mut Repl<'fixed>)`, which cannot coerce to the higher-ranked
/// `for<'a, 'b> fn(&'b mut Repl<'a>)` the field demands. The closure coerces at
/// a flexible inference site, where the higher-ranked target is satisfied.
const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        aliases: &[],
        help: "List the available commands.",
        run: |r| r.cmd_help(),
    },
    SlashCommand {
        name: "/clear",
        aliases: &[],
        help: "Forget the conversation and clear the screen.",
        run: |r| r.cmd_clear(),
    },
    SlashCommand {
        name: "/model",
        aliases: &[],
        help: "Switch the model or provider.",
        run: |r| r.cmd_model(),
    },
    SlashCommand {
        name: "/compact",
        aliases: &[],
        help: "Summarize the conversation to reclaim context.",
        run: |r| r.cmd_compact(),
    },
    SlashCommand {
        name: "/quit",
        aliases: &["/exit"],
        help: "Leave exarch.",
        run: |r| r.cmd_quit(),
    },
];

/// The command whose name or alias exactly equals `trimmed`, if any.
fn lookup_command(trimmed: &str) -> Option<&'static SlashCommand> {
    SLASH_COMMANDS
        .iter()
        .find(|c| c.name == trimmed || c.aliases.contains(&trimmed))
}

/// Whether `text`, as typed, is a recognized slash command — trimmed and
/// matched whole, mirroring [`Repl::handle_slash`]'s exact-match dispatch.
fn is_slash_command(text: &str) -> bool {
    lookup_command(text.trim()).is_some()
}

impl Repl<'_> {
    fn drive(&mut self, seed: Option<String>) -> Result<(), String> {
        let mut pending = seed;
        loop {
            // Order of sources: the seed prompt, then queued prompts the
            // worker did not already drain at a tool boundary, then a fresh
            // blocking read. Queued prompts flow through `handle_slash` like
            // any other, so they echo as they are sent and a lone `/clear`
            // still works when it reaches the ordinary turn boundary.
            let prompt = match pending.take() {
                Some(p) => Some(p),
                None => match self.tui.app.take_queue() {
                    Some(q) => Some(q),
                    None => match read_prompt(self.tui.guard.term(), &mut self.tui.app)
                        .map_err(|e| e.to_string())?
                    {
                        Some(s) => Some(s),
                        None => return Ok(()),
                    },
                },
            };
            if let Some(text) = &prompt {
                match self.handle_slash(text) {
                    Slash::Quit => return Ok(()),
                    Slash::Continue => continue,
                    Slash::Prompt => {}
                }
            }
            self.session
                .run_turn(&mut self.tui, &self.provider, prompt)?;
            self.tui
                .app
                .draw(self.tui.guard.term())
                .map_err(|e| e.to_string())?;
        }
    }

    fn handle_slash(&mut self, text: &str) -> Slash {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Slash::Continue;
        }
        if let Some(cmd) = lookup_command(trimmed) {
            return (cmd.run)(self);
        }
        // Not a command: an ordinary prompt. Echo it onto the transcript;
        // `session.append_user` (inside `apply`) is what records it model-side.
        let id = self.session.id;
        self.tui.handle(Event {
            id,
            kind: Kind::UserPromptEcho(text.to_string()),
        });
        Slash::Prompt
    }

    fn cmd_quit(&mut self) -> Slash {
        Slash::Quit
    }

    fn cmd_clear(&mut self) -> Slash {
        let id = self.session.id;
        if let Err(e) = self.session.clear(self.scratch) {
            self.note_error(id, format!("could not clear session: {e}"));
            return Slash::Continue;
        }
        if let Err(e) = self.tui.app.clear(self.info, self.tui.guard.term()) {
            self.note_error(id, format!("could not redraw after /clear: {e}"));
        }
        Slash::Continue
    }

    fn cmd_model(&mut self) -> Slash {
        self.pick_model();
        Slash::Continue
    }

    fn cmd_compact(&mut self) -> Slash {
        let id = self.session.id;
        // Publish a root token for the duration of the summarize, as
        // `run_turn` does: without it `cancel::is_set()` reads the null
        // slot and the provider's mid-stream cancel race never fires, so
        // Esc could not abort the in-flight summarize request.
        let _root = cancel::mint_root();
        let provider = &self.provider;
        let session = &mut *self.session;
        let _ = pump(&mut self.tui, id, |emit| {
            session.compact(provider, emit, true)
        });
        Slash::Continue
    }

    /// Emit one dim transcript line per registry entry: the command token
    /// (with aliases) left-padded to a common width, then its description.
    fn cmd_help(&mut self) -> Slash {
        let id = self.session.id;
        let names: Vec<String> = SLASH_COMMANDS
            .iter()
            .map(|c| {
                if c.aliases.is_empty() {
                    c.name.to_string()
                } else {
                    format!("{} ({})", c.name, c.aliases.join(", "))
                }
            })
            .collect();
        let width = names.iter().map(String::len).max().unwrap_or(0);
        for (n, c) in names.iter().zip(SLASH_COMMANDS) {
            self.tui.handle(Event {
                id,
                kind: Kind::Dim(format!("{n:<width$}   {}", c.help)),
            });
        }
        Slash::Continue
    }

    /// Open the `/model` picker over the available providers, fetch their
    /// model lists (cache-first, then background), and drive the modal loop
    /// until the user selects a model or dismisses it. On a selection the
    /// provider is rebuilt over the same transcript, the saved selection is updated,
    /// and the status bar follows.
    fn pick_model(&mut self) {
        let available = self.store.available();
        let subscription = available
            .iter()
            .copied()
            .filter(|&k| self.store.get(k).is_some_and(|c| c.is_subscription()))
            .collect();
        let mut picker = Picker::new(available, subscription);
        // Seed each provider from the catalog's cache instantly; spawn a
        // background fetch for the rest so the UI shows "loading…" rather
        // than freezing on the network. A subscription provider has no
        // catalog endpoint, so its curated plan models are seeded directly
        // and it is excluded from the fetch.
        let mut rx = None;
        let to_fetch: Vec<_> = picker
            .loading_providers()
            .into_iter()
            .filter(|&k| {
                if self.store.get(k).is_some_and(|c| c.is_subscription()) {
                    let models = crate::oauth::PLAN_MODELS
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                    picker.set_models(k, picker::ModelsState::Loaded(models));
                    return false;
                }
                match self.catalog.cached(k) {
                    Some(models) => {
                        picker.set_models(k, picker::ModelsState::Loaded(models));
                        false
                    }
                    None => true,
                }
            })
            .collect();
        if !to_fetch.is_empty() {
            let (tx, recv) = std::sync::mpsc::channel();
            for kind in to_fetch {
                let source = self.catalog.source().clone();
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let result = source.list(kind);
                    let _ = tx.send((kind, result));
                });
            }
            rx = Some(recv);
        }
        self.tui.app.picker = Some(picker);
        let outcome = self.drive_picker(rx);
        self.tui.app.picker = None;
        if let Some((kind, model)) = outcome {
            self.apply_model_switch(kind, model);
        }
    }

    /// Poll keys and background-fetch results until the picker resolves.
    /// Returns the chosen `(provider, model)`, or `None` on cancel.
    fn drive_picker(&mut self, rx: Option<FetchRx>) -> Option<(provider::ProviderKind, String)> {
        loop {
            // Fold any landed fetch results into the picker (and the
            // catalog's caches), on this thread, so the disk write stays
            // single-threaded.
            if let Some(rx) = &rx {
                while let Ok((kind, result)) = rx.try_recv() {
                    let state = match result {
                        Ok(models) => {
                            self.catalog.record(kind, models.clone());
                            picker::ModelsState::Loaded(models)
                        }
                        Err(reason) => picker::ModelsState::Failed(reason),
                    };
                    if let Some(p) = self.tui.app.picker_mut() {
                        p.set_models(kind, state);
                    }
                }
            }
            if self.tui.app.draw(self.tui.guard.term()).is_err() {
                return None;
            }
            if !ct_poll(Duration::from_millis(100)).unwrap_or(false) {
                continue;
            }
            let Ok(CtEvent::Key(k)) = ct_read() else {
                continue;
            };
            if k.kind != KeyEventKind::Press {
                continue;
            }
            let action = self.tui.app.picker_mut()?.key(k.code);
            match action {
                picker::PickAction::None => {}
                picker::PickAction::Cancelled => return None,
                picker::PickAction::Selected(kind, model) => return Some((kind, model)),
                picker::PickAction::Manual(query) => {
                    let available = self.store.available();
                    match crate::models::resolve_model_provider(&query, &available, self.catalog) {
                        Ok(kind) => return Some((kind, query)),
                        Err(e) => self.note_error(self.session.id, e),
                    }
                }
            }
        }
    }

    /// Rebuild the provider for the chosen `kind` + `model` over the same
    /// transcript, persist the selection to the project state dir, and
    /// update the live status bar. A persistence failure is noted but does
    /// not undo the in-memory switch.
    fn apply_model_switch(&mut self, kind: provider::ProviderKind, model: String) {
        let id = self.session.id;
        let Some(cred) = self.store.get(kind).cloned() else {
            self.note_error(id, format!("{} has no resolved credential", kind.info().0));
            return;
        };
        self.provider = Provider::build(kind, model.clone(), &cred, self.info.max_tokens_override);
        let label = kind.info().0;
        let status_provider = crate::oauth::provider_label(self.provider.is_subscription(), label);
        self.tui.app.set_status_model(&status_provider, &model);
        let state_dir = crate::bootstrap::project_dir(self.info.cwd);
        if let Err(e) = state::save(&state_dir, &state::State::new(kind, &model)) {
            self.note_error(id, format!("could not persist selection: {e}"));
        }
        self.tui.handle(Event {
            id,
            kind: Kind::Dim(format!("[switched to {label} {model}]")),
        });
    }

    fn note_error(&mut self, id: SessionId, message: String) {
        self.tui.handle(Event {
            id,
            kind: Kind::Error(message),
        });
    }
}

/// Drain `rx` and poll terminal keys at ~60 FPS until `rx`
/// disconnects.  Keystrokes go into the input editor (the user composes a
/// steering prompt during the turn); on the main tab Enter queues the draft
/// (`App::enqueue`) for the worker to drain at the next tool boundary, or for
/// the REPL to send at the turn boundary if no such boundary arrives.  Ctrl-C /
/// Esc raise the cancel flag.  The drain is batched so terminal polling never
/// starves during heavy token streaming.
fn drive_events(term: &mut Term, app: &mut App, rx: Receiver<Event>) -> io::Result<()> {
    const BATCH: usize = 64;
    const MIN_FRAME_MS: u64 = 16; // ~60 FPS max
    loop {
        let mut more = true;
        for _ in 0..BATCH {
            match rx.try_recv() {
                Ok(ev) => app.handle(ev),
                Err(TryRecvError::Empty) => {
                    more = false;
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    app.draw(term)?;
                    return Ok(());
                }
            }
        }
        app.tick();
        app.draw(term)?;
        // Poll for input every iteration, even with events still
        // queued: a backlog of streamed tokens must never starve
        // Esc/Ctrl-C. While the drain is incomplete the poll is
        // non-blocking so draining stays prompt; once the channel is
        // empty it waits up to a frame for the next key.
        let before = std::time::Instant::now();
        let timeout = if more {
            Duration::ZERO
        } else {
            Duration::from_millis(MIN_FRAME_MS)
        };
        if ct_poll(timeout)? {
            match ct_read()? {
                CtEvent::Key(k) if k.kind == KeyEventKind::Press => {
                    let ctrl_c =
                        k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
                    let queue_submit = k.code == KeyCode::Enter
                        && app.focused() == app.root
                        && !k.modifiers.contains(KeyModifiers::SHIFT)
                        && !k.modifiers.contains(KeyModifiers::ALT);
                    let ctrl_backslash = k.code == KeyCode::Char('\\')
                        && k.modifiers.contains(KeyModifiers::CONTROL);
                    if ctrl_backslash {
                        // Reap the whole session: cancel the durable root.
                        cancel::raise_root_abort();
                    } else if ctrl_c || k.code == KeyCode::Esc {
                        cancel::raise_interrupt();
                    } else if queue_submit {
                        app.enqueue();
                    } else {
                        app.key(k);
                    }
                }
                CtEvent::Paste(s) => app.paste(&s),
                CtEvent::Mouse(m) => app.mouse(m),
                _ => {}
            }
        }
        // Cap the frame rate only when idle; while a backlog drains the
        // sleep is skipped so throughput isn't throttled to
        // BATCH * (1000 / MIN_FRAME_MS) events per second.
        if !more {
            let elapsed = before.elapsed().as_millis() as u64;
            if elapsed < MIN_FRAME_MS {
                std::thread::sleep(Duration::from_millis(MIN_FRAME_MS - elapsed));
            }
        }
    }
}

fn read_prompt(term: &mut Term, app: &mut App) -> io::Result<Option<String>> {
    loop {
        app.tick();
        app.draw(term)?;
        if !ct_poll(Duration::from_millis(100))? {
            continue;
        }
        match ct_read()? {
            CtEvent::Key(k) => {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                // Ctrl-D quits from anywhere.
                if k.code == KeyCode::Char('d') && k.modifiers.contains(KeyModifiers::CONTROL) {
                    return Ok(None);
                }
                // Submit (Enter / Ctrl-C-as-submit) is gated on main:
                // non-main tabs are watch-only and route everything but
                // Tab through `app.key` (which itself ignores typing
                // off-main).
                if app.focused() != app.root {
                    app.key(k);
                    continue;
                }
                match (k.code, k.modifiers) {
                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                        if app.submit().is_some() {
                        } else {
                            return Ok(None);
                        }
                    }
                    (KeyCode::Enter, m)
                        if !m.contains(KeyModifiers::SHIFT) && !m.contains(KeyModifiers::ALT) =>
                    {
                        if let Some(s) = app.submit() {
                            return Ok(Some(s));
                        }
                    }
                    _ => {
                        app.key(k);
                    }
                }
            }
            CtEvent::Paste(s) => app.paste(&s),
            CtEvent::Mouse(m) => app.mouse(m),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Hunk, Kind};

    #[test]
    fn slash_command_recognition() {
        assert!(is_slash_command("/clear"));
        assert!(is_slash_command("/quit"));
        assert!(is_slash_command("/exit")); // alias of /quit
        assert!(is_slash_command("/help"));
        assert!(is_slash_command("  /model  "));
        assert!(!is_slash_command("/clearx"));
        assert!(!is_slash_command("/clear now"));
        assert!(!is_slash_command("/unknown"));
        assert!(!is_slash_command(""));
        assert!(!is_slash_command("hello"));
    }

    /// Every registry token is a distinct, well-formed slash command.
    #[test]
    fn registry_is_well_formed() {
        use std::collections::HashSet;
        let mut seen = HashSet::new();
        for c in SLASH_COMMANDS {
            assert!(!c.help.is_empty(), "command {} needs help text", c.name);
            for tok in std::iter::once(c.name).chain(c.aliases.iter().copied()) {
                assert!(
                    tok.starts_with('/'),
                    "command token {tok:?} must start with '/'"
                );
                assert!(seen.insert(tok), "duplicate command token {tok:?}");
            }
        }
    }

    /// A `Kind::Patch` routed through `App::handle` with the root
    /// session id must commit a `❖ patch <path>` line into the root
    /// viewport's scrollback once the buffer flushes.  Patches are
    /// now grouped — a single Patch event sits in [`App::patch_buf`]
    /// until a non-Patch event arrives, so the test fires a sentinel
    /// `Kind::Step` to trigger the flush before inspecting the
    /// committed buffer.
    #[test]
    fn kind_patch_lands_in_committed_scrollback() {
        let (mut app, root_id, tmp) = fresh_app();
        app.handle(Event {
            id: root_id,
            kind: Kind::Patch {
                path: "examples/hello.ral".into(),
                hunk: Hunk {
                    start: 1,
                    before: vec![],
                    del: vec!["original".into()],
                    add: vec!["NEW".into()],
                    after: vec![],
                },
            },
        });
        app.handle(Event {
            id: root_id,
            kind: Kind::Step(2),
        });
        let texts = committed_text(&mut app, root_id);
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(
            texts
                .iter()
                .any(|t| t.contains("patch") && t.contains("examples/hello.ral")),
            "expected a `❖ patch examples/hello.ral` line in committed; got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("- original")),
            "expected a `- original` removed-line row; got: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t.contains("+ NEW")),
            "expected a `+ NEW` added-line row; got: {texts:?}"
        );
    }

    /// Three consecutive `Kind::Patch` events on the same `(id, path)`
    /// must render under ONE `❖ patch` header as three located hunks in
    /// arrival order — the way a unified diff presents several changes to
    /// one file — rather than as three separate header blocks.  Each hunk
    /// keeps its own `del`-then-`add` ordering, so the rows read
    /// `old-1 new-1 old-2 new-2 old-3 new-3`.  A non-Patch event after
    /// the run triggers the flush.
    #[test]
    fn consecutive_patches_group_under_one_header() {
        let (mut app, root_id, tmp) = fresh_app();
        for i in 1..=3 {
            app.handle(Event {
                id: root_id,
                kind: Kind::Patch {
                    path: "/tmp/austen.txt".into(),
                    hunk: Hunk {
                        start: i * 10,
                        before: vec![],
                        del: vec![format!("old-{i}")],
                        add: vec![format!("new-{i}")],
                        after: vec![],
                    },
                },
            });
        }
        // A non-Patch event flushes the buffer.
        app.handle(Event {
            id: root_id,
            kind: Kind::Step(2),
        });
        let texts = committed_text(&mut app, root_id);
        let _ = std::fs::remove_dir_all(&tmp);
        let headers = texts
            .iter()
            .filter(|t| t.contains("patch") && t.contains("/tmp/austen.txt"))
            .count();
        assert_eq!(
            headers, 1,
            "expected exactly one `❖ patch /tmp/austen.txt` header; got {headers} in {texts:?}"
        );
        // Each hunk's removed line precedes its own added line, and the
        // hunks stay in arrival order.
        let row = |needle: &str| {
            texts
                .iter()
                .position(|t| t.contains(needle))
                .unwrap_or_else(|| panic!("missing `{needle}` row in {texts:?}"))
        };
        let order = [
            row("- old-1"),
            row("+ new-1"),
            row("- old-2"),
            row("+ new-2"),
            row("- old-3"),
            row("+ new-3"),
        ];
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "hunks must render del-then-add in arrival order; got {order:?} in {texts:?}"
        );
    }

    /// A second `Kind::Patch` targeting a *different* path closes the
    /// first block and opens a new one — paths don't merge across the
    /// `(id, path)` key.  Two `❖ patch` headers must land.
    #[test]
    fn patches_on_different_paths_dont_merge() {
        let (mut app, root_id, tmp) = fresh_app();
        app.handle(Event {
            id: root_id,
            kind: Kind::Patch {
                path: "a.txt".into(),
                hunk: Hunk {
                    start: 1,
                    before: vec![],
                    del: vec!["a-old".into()],
                    add: vec!["a-new".into()],
                    after: vec![],
                },
            },
        });
        app.handle(Event {
            id: root_id,
            kind: Kind::Patch {
                path: "b.txt".into(),
                hunk: Hunk {
                    start: 1,
                    before: vec![],
                    del: vec!["b-old".into()],
                    add: vec!["b-new".into()],
                    after: vec![],
                },
            },
        });
        app.handle(Event {
            id: root_id,
            kind: Kind::Step(2),
        });
        let texts = committed_text(&mut app, root_id);
        let _ = std::fs::remove_dir_all(&tmp);
        let headers: Vec<&String> = texts
            .iter()
            .filter(|t| t.starts_with(line::RAIL) && t.contains("patch"))
            .collect();
        assert_eq!(
            headers.len(),
            2,
            "expected two `❖ patch` headers (one per path); got {headers:?} in {texts:?}"
        );
    }

    // ── shared test helpers ────────────────────────────────────────────

    fn fresh_app() -> (App, SessionId, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!(
            "exarch-app-patch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::create_dir_all(&tmp);
        let root_id: SessionId = 1;
        let app = App::new(root_id, &tmp, None);
        (app, root_id, tmp)
    }

    fn committed_text(app: &mut App, id: SessionId) -> Vec<String> {
        app.viewports
            .get_mut(&id)
            .expect("viewport must exist")
            .flatten_text(READ_W)
    }

    /// Enter while a turn is in flight queues the draft instead of running it;
    /// boundary drains coalesce oldest-first into one prompt joined by a blank
    /// line. An empty draft queues nothing, and draining leaves the queue empty
    /// so the next turn starts clean.
    #[test]
    fn enqueue_coalesces_in_order_then_drains_empty() {
        let (mut app, _root, tmp) = fresh_app();
        app.set_prompt("first");
        assert!(app.enqueue());
        app.set_prompt("second");
        assert!(app.enqueue());
        assert!(!app.enqueue(), "an empty draft queues nothing");
        let drained = app.take_queue();
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(drained.as_deref(), Some("first\n\nsecond"));
        assert_eq!(app.take_queue(), None, "draining empties the queue");
    }

    /// Bare Up on an empty prompt recalls the newest pending prompt for
    /// editing and removes only that prompt from the pending queue.
    #[test]
    fn up_recalls_newest_queued_prompt_for_editing() {
        let (mut app, _root, tmp) = fresh_app();
        app.set_prompt("first");
        assert!(app.enqueue());
        app.set_prompt("second");
        assert!(app.enqueue());

        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

        assert_eq!(app.prompt_text(), "second");
        assert_eq!(app.queue.snapshot(), vec!["first".to_string()]);
        app.set_prompt("second edited");
        assert!(app.enqueue());
        assert_eq!(app.take_queue().as_deref(), Some("first\n\nsecond edited"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// `tail_bytes` keeps the most-recent content within the cap and never
    /// splits a multi-byte char (a naive byte slice would panic).
    #[test]
    fn tail_bytes_bounds_to_tail_on_a_char_boundary() {
        assert_eq!(tail_bytes("short", 6000), "short");
        // Each `é` is two bytes; a cap landing mid-char snaps forward so the
        // returned slice is valid UTF-8 and no wider than the cap.
        let s = "é".repeat(100);
        let t = tail_bytes(&s, 5);
        assert!(t.len() <= 5);
        assert_eq!(t, "éé");
    }
}
