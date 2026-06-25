//! Full-screen TUI frontend.
//!
//! One [`Sink`] implementation, plus the REPL loop the user types into.
//! The TUI owns raw-mode, bracketed-paste, the alternate screen, and
//! mouse capture through [`TerminalGuard`]; the agent core in
//! [`crate::bus`] and [`crate::agent`] sees only an
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
mod fidelity;
mod group;
mod highlight;
mod line;
mod md;
mod picker;
mod rail;
mod viewport;
use block::{AgentSlot, RailShape};
use fidelity::Fidelity;
use rail::RailKind;

use line::usage_text;

use crate::agent::{Agent, Control, ControlFlow};
use crate::agent_registry::AgentRegistry;
use crate::bootstrap::Scratch;
use crate::bus::{
    AgentId, Emitter, Event, FleetBus, Hunk, Inbox, InboxMsg, Kind, Mailbox, Pass, drain_pass,
};
use crate::cancel;
use crate::card::{Card, Field, FieldVal, IoEvent, IoKind, Mark, Role, Span as CardSpan};
use crate::credential::CredentialStore;
use crate::fleet::{Fleet, NO_FOCUS};
use crate::models::{LiveSource, ModelCatalog, ModelSource};
use crate::provider::{self, Provider, Usage};
use crate::state;
use ral_core::path::sigil::expand_path_prefix;
use std::sync::Arc;

use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::Print,
    terminal::{
        BeginSynchronizedUpdate, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode, size,
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
    widgets::Paragraph,
};
use ratatui_textarea::TextArea;
use std::{
    collections::HashMap,
    io::{self, Stdout},
    path::{Path, PathBuf},
    sync::{
        Once,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use textarea_vim::{Mode, Vim, place_native_cursor};

use line::{
    AGENT_HUES, BANNER_GOLD, BANNER_PINK, CYAN, OVERLAY_BG, PINK, PURPLE, RAIL_W,
    READ_W, SLATE, bold,
};
use viewport::Viewport;

pub(super) const PROMPT_PAD_H: u16 = 1;

/// Left gutter for the transcript, queued-prompt strip, and rule line.
/// Gives the marginal rail breathing room from the terminal edge so it
/// reads as a Bertin data column rather than frame chrome.
const LEFT_MARGIN: u16 = 2;
/// Width of the pinned-state register column, in columns — a framed gauge
/// (`│ tasks ▓▓░ 3/8 │`) plus its borders and a padding column each side.
const REGISTER_W: u16 = 28;
/// Minimum reading gap between the `READ_W`-capped transcript and the register
/// column.  The register is reserved only when the content area is at least
/// `LEFT_MARGIN + READ_W + REGISTER_GAP + REGISTER_W` wide — wide enough that
/// reclaiming the dead right margin costs the transcript nothing; below that it
/// collapses to the pin band.
const REGISTER_GAP: u16 = 4;
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

static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whether the kitty keyboard-enhancement flags are currently pushed, so the
/// matching pop runs exactly once even when the panic hook and `Drop` both
/// reach [`restore_terminal_modes`] on an unwind.
static KBD_ENHANCED: AtomicBool = AtomicBool::new(false);
static PANIC_RESTORE_HOOK: Once = Once::new();

fn install_panic_restore_hook() {
    PANIC_RESTORE_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if TUI_ACTIVE.swap(false, Ordering::AcqRel) {
                restore_terminal_modes();
            }
            previous(info);
        }));
    });
}

/// Apply the raw-mode + alternate-screen + bracketed-paste + mouse-capture
/// modes to the current `stdout`, and opt into the kitty keyboard protocol.
/// Split out from [`enter_terminal_modes`] so the editor hatch
/// ([`compose_in_editor`]) can re-enter the same modes after suspending the
/// TUI for a child editor, without building a second [`Term`].
fn apply_terminal_modes() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        // Any-motion mouse reporting (DECSET 1003), on top of the button
        // tracking `EnableMouseCapture` turns on: the terminal reports
        // pointer motion with no button held, so the hover-dial glyph can
        // track the pointer.  Crossterm has no typed command for 1003, so
        // the sequence is emitted raw; `restore_terminal_modes` pops it.
        Print("\x1b[?1003h"),
    )?;
    // Without the enhancement protocol the Meta/Alt chords the emacs keymap
    // binds — M-f, M-b, M-d, M-<, M-> — never reach crossterm as ALT events.
    // Terminals that do not implement it ignore the sequence; the matching pop
    // in `restore_terminal_modes` is gated on `KBD_ENHANCED` so it stays
    // balanced either way.
    execute!(
        io::stdout(),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )?;
    KBD_ENHANCED.store(true, Ordering::Release);
    Ok(())
}

fn enter_terminal_modes() -> io::Result<Term> {
    apply_terminal_modes()?;
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    term.hide_cursor()?;
    Ok(term)
}

fn restore_terminal_modes() {
    if KBD_ENHANCED.swap(false, Ordering::AcqRel) {
        let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        io::stdout(),
        Show,
        // Pop any-motion reporting (1003) before the rest of mouse capture,
        // balancing the raw enable in `apply_terminal_modes`.
        Print("\x1b[?1003l"),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

/// Resolve the editor to launch for `C-x C-e`: `$VISUAL`, then `$EDITOR`,
/// then `vi`.  The value is split on whitespace so a spec like `emacsclient
/// -t` or `code --wait` keeps its arguments.
fn editor_command() -> (String, Vec<String>) {
    let spec = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_string());
    let mut parts = spec.split_whitespace().map(str::to_string);
    let program = parts.next().unwrap_or_else(|| "vi".to_string());
    (program, parts.collect())
}

/// Compose `draft` in the user's `$EDITOR`: write it to a scratch file, suspend
/// the TUI's terminal modes so the child editor owns the tty, run it, then
/// re-enter and read the result back.  Returns the edited text with one
/// trailing newline trimmed (the prompt is newline-joined, so the editor's
/// final newline would otherwise add a blank last row), or `None` when the
/// editor could not be launched, exited non-zero, or left nothing readable —
/// in every such case the original draft is kept.  Only a failure to re-enter
/// the terminal modes (a broken tty) propagates, since the TUI cannot continue.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:editor-compose] writes the prompt draft to a scratch file, spawns the user's $EDITOR on it, and reads it back for the C-x C-e hatch; a UI action on a temp file, not turn-time model I/O"
)]
fn edit_text_in_editor(draft: &str) -> io::Result<Option<String>> {
    let path = std::env::temp_dir().join(format!("exarch-prompt-{}.md", std::process::id()));
    if std::fs::write(&path, draft).is_err() {
        return Ok(None);
    }
    let (program, args) = editor_command();

    restore_terminal_modes();
    let status = std::process::Command::new(&program)
        .args(&args)
        .arg(&path)
        .status();
    apply_terminal_modes()?;

    let edited = match status {
        Ok(s) if s.success() => match std::fs::read_to_string(&path) {
            Ok(text) => {
                let text = text.strip_suffix('\n').unwrap_or(&text);
                Some(text.strip_suffix('\r').unwrap_or(text).to_string())
            }
            Err(_) => None,
        },
        _ => None,
    };
    let _ = std::fs::remove_file(&path);
    Ok(edited)
}

/// Drive the `C-x C-e` editor hatch from the UI loop, which owns the terminal
/// the editor must borrow.  Adopts the edited text as the prompt draft and
/// forces a full repaint over whatever the editor left on the screen.
fn compose_in_editor(tui: &mut Tui) -> io::Result<()> {
    let draft = tui.app.prompt_text();
    if let Some(edited) = edit_text_in_editor(&draft)? {
        tui.app.adopt_draft(&edited);
    }
    tui.guard.term().clear()?;
    Ok(())
}

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
        install_panic_restore_hook();
        TUI_ACTIVE.store(true, Ordering::Release);
        #[cfg(unix)]
        let mut stderr_backup = Some(match redirect_stderr_to_file(stderr_log) {
            Ok(backup) => backup,
            Err(e) => {
                TUI_ACTIVE.store(false, Ordering::Release);
                return Err(e);
            }
        });
        let term = match enter_terminal_modes() {
            Ok(term) => term,
            Err(e) => {
                restore_terminal_modes();
                TUI_ACTIVE.store(false, Ordering::Release);
                #[cfg(unix)]
                if let Some(backup) = stderr_backup.take() {
                    restore_stderr(backup);
                }
                return Err(e);
            }
        };
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
        restore_terminal_modes();
        TUI_ACTIVE.store(false, Ordering::Release);
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:stderr-log] opens the TUI debug log for fd-2 redirect; trace infra, not turn-time data I/O"
)]
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

/// How the agent×step matrix orders its rows — a render-time projection
/// of the same `tabs`/`viewports` model, never a reshuffle of the
/// underlying state.  [`MatrixSort::Spawn`] is the default (the `tabs`
/// order, root first then subagents as born); [`MatrixSort::Cost`]
/// surfaces the budget-burner by sorting on cumulative token spend.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum MatrixSort {
    #[default]
    Spawn,
    Cost,
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
    viewports: HashMap<AgentId, Viewport>,
    /// Insertion order of viewports — root first, then subagents as
    /// they were born.  Drives [`Self::flush_logs`] for stable
    /// per-session log paths across runs.
    dispatch_order: Vec<AgentId>,
    /// Tabs visible in the tab bar.  Always starts with `root`; sub-
    /// agents are appended on `Born` and removed when their entry in
    /// `dying` ages out past [`LINGER`].
    tabs: Vec<AgentId>,
    /// Per-session label.  Root maps to [`ROOT_TITLE`]; subagents to
    /// the `title` field of their `Kind::Born` event.
    titles: HashMap<AgentId, String>,
    /// Death timestamps for subagents in their linger window.  Tabs
    /// drop from [`Self::tabs`] once [`LINGER`] elapses; the viewport
    /// stays alive for log flushing.
    dying: HashMap<AgentId, Instant>,
    root: AgentId,
    /// The fleet's focused-agent handle, shared with the agents' drive loops
    /// (an [`AtomicU64`] of an [`AgentId`], or `NO_FOCUS`).  `TAB` stores into
    /// it; the focused agent reads it in its park predicate and parks for the
    /// human.  Reads route through [`Self::focused`] so a stale id (an expired
    /// tab, or the no-focus sentinel) resolves to root.  Bound to the trunk's
    /// shared handle by [`Self::bind_focus`].
    focus: Arc<AtomicU64>,
    /// Each tab's parent (the spawning agent), recorded from `Kind::Born`, so
    /// focus can fall back to the parent — recursing toward the trunk — when a
    /// focused agent ends.
    parents: HashMap<AgentId, AgentId>,
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
    /// The session's own inbox, bound here by [`Self::bind_inbox`] at REPL
    /// start so the input editor, the pending strip, and the worker's drive
    /// loop all share one queue. A submitted prompt is pushed onto it (through a
    /// `Mailbox`); the worker drains a non-slash prefix after a tool result to
    /// steer the next assistant step ([`Agent::dispatch`]) and the remainder
    /// at the next turn boundary ([`Inbox::next_or_idle`]). Until drained,
    /// pending messages render in the strip above the input
    /// ([`line::queued_prompt`]) and bare Up on an empty prompt pulls the
    /// whole queued run back into the editor for revision.
    inbox: Inbox,
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
    /// In-flight aggregation of consecutive single-`diff` cards targeting
    /// the same `(session, path)`.  Each `edit` invocation surfaces its own
    /// `` `card `` carrying one `diff` mark; without grouping, ten
    /// consecutive edits to one file would render as ten separate
    /// `▎ diff <path>` blocks rather than one block with ten located hunks,
    /// the way a unified diff presents a single file.  The buffer
    /// accumulates hunks until any non-diff content lands (next tool call,
    /// assistant text, step boundary, a richer card, session death), at
    /// which point [`Self::flush_patch_buf`] emits one block.
    patch_buf: Option<PatchBuf>,
    /// In-flight aggregation of consecutive structural I/O surfaces — even
    /// interleaved — into one block *per kind*.  Core emits one
    /// `Kind::Io { event, .. }` per effect; a burst of reads and execs would
    /// otherwise render as `Read…, $…, Read…, $…` clutter.  The buffer
    /// buckets events by kind (reads, execs, greps, writes), deduped and
    /// order-independent, until any non-io content lands (the shared
    /// [`Self::flush_surfaces`] boundary, or a session change), at which point
    /// [`Self::flush_io_buf`] emits one block per non-empty bucket.
    io_buf: Option<IoBuf>,
    /// Geometry of the content area as of the last [`Self::draw`], so a
    /// mouse event arriving between frames maps to a buffer row.
    frame: Option<FrameGeom>,
    /// Active linewise drag-selection in focused-viewport row
    /// coordinates, painted reversed and copied on release.
    selection: Option<(usize, usize)>,
    /// In-flight left-button gesture: the row pressed, the block under
    /// it, and whether the pointer has since moved (a drag, not a click).
    press: Option<Press>,
    /// The dialable block the pointer currently rests over, if any — its
    /// rail glyph is painted brightened so the dial target is legible
    /// without hunting.  Tracked from pointer motion (any-motion mouse
    /// reporting, enabled in [`apply_terminal_modes`]) and cleared when the
    /// pointer leaves every dialable block.
    hover: Option<usize>,
    /// How the multi-agent matrix orders its rows — toggled by `BackTab`
    /// (Shift+Tab) when more than one session is live.  A render-time
    /// projection; the `tabs`/focus model is untouched.
    matrix_sort: MatrixSort,
    /// Vi-mode editing state for the prompt, or `None` in the default
    /// emacs-style mode.  When `Some`, plain text input routes through the
    /// shared [`textarea_vim`] state machine instead of straight to the
    /// textarea; when `None`, the prompt edits exactly as it did before vi
    /// mode existed.  Started in [`Mode::Insert`] (see [`App::new`]).
    vim: Option<Vim>,
    /// The first key of a `C-x …` editor-command chord has been seen; the next
    /// keystroke completes or cancels it (see [`App::key`]).
    cx_pending: bool,
    /// `C-x C-e` was pressed: the UI loop should suspend the TUI and compose the
    /// prompt in `$EDITOR`.  Set here, drained by [`App::take_editor_request`].
    editor_request: bool,
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
    /// Block under the press, cycled on a rail click that never dragged.
    block: Option<usize>,
    /// Whether the press landed on the rail column (cols 0–1), where a
    /// click cycles the block; a click off the rail stays selection.
    on_rail: bool,
    dragged: bool,
}

/// Accumulator backing [`App::patch_buf`].
struct PatchBuf {
    id: AgentId,
    path: String,
    hunks: Vec<Hunk>,
}

/// Accumulator backing [`App::io_buf`].  Buckets consecutive structural I/O
/// surfaces by kind, deduped and order-independent (the user does not care
/// about interleave order); flushed as one block per non-empty bucket.  The
/// exec/grep/write buckets keep the typed [`IoEvent`] rather than pre-rendered
/// spans so flush-time rendering can reuse the exact `io_card` span idioms via
/// [`crate::card::io_group_card`].
struct IoBuf {
    id: AgentId,
    /// Read paths, first-seen order, deduped.
    reads: Vec<String>,
    /// `Exec` events, deduped by `argv`.
    execs: Vec<IoEvent>,
    /// `Grep` events, deduped by `(scope, pattern)`.
    greps: Vec<IoEvent>,
    /// `Write` events, deduped by `path` (keeping the latest outcome).
    writes: Vec<IoEvent>,
}

impl App {
    pub fn new(
        root_id: AgentId,
        root_log_dir: &Path,
        context_window: Option<u64>,
        vi: bool,
    ) -> Self {
        let mut viewports = HashMap::new();
        viewports.insert(
            root_id,
            Viewport::new(root_log_dir.join("user.log"), AgentSlot::default()),
        );
        let mut titles = HashMap::new();
        titles.insert(root_id, ROOT_TITLE.to_string());
        let mut textarea = TextArea::default();
        textarea.set_wrap_mode(ratatui_textarea::WrapMode::WordOrGlyph);
        textarea.set_style(Style::default().fg(Color::White));
        textarea.set_cursor_line_style(Style::default().fg(Color::White));
        // Suppress the widget's painted cursor cell (plain style): the prompt
        // shows the terminal's own (native, blinking) cursor in every mode,
        // positioned each frame by `place_native_cursor`.
        textarea.set_cursor_style(Style::default());
        textarea.set_block(
            ratatui::widgets::Block::default()
                .borders(ratatui::widgets::Borders::ALL)
                .border_type(ratatui::widgets::BorderType::Rounded)
                .border_style(Style::default().fg(PINK))
                .padding(ratatui::widgets::Padding::horizontal(PROMPT_PAD_H)),
        );
        // Vi mode opens in insert, so editing starts where an emacs user
        // would expect.
        let vim = vi.then(|| Vim::new(Mode::Insert));
        Self {
            viewports,
            dispatch_order: vec![root_id],
            tabs: vec![root_id],
            titles,
            dying: HashMap::new(),
            root: root_id,
            // A placeholder until [`Self::bind_focus`] wires the trunk's shared
            // handle; `focused()` resolves the no-focus sentinel to root.
            focus: Arc::new(AtomicU64::new(NO_FOCUS)),
            parents: HashMap::new(),
            textarea,
            history: Vec::new(),
            hist_pos: None,
            draft: String::new(),
            inbox: Inbox::new(),
            total_usage: Usage::default(),
            patch_buf: None,
            io_buf: None,
            last_input: 0,
            context_window,
            status_model: String::new(),
            picker: None,
            frame: None,
            selection: None,
            press: None,
            hover: None,
            matrix_sort: MatrixSort::default(),
            vim,
            cx_pending: false,
            editor_request: false,
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
    pub fn set_status_model(&mut self, provider: &str, model: &str) {
        self.status_model = format!("{provider} {model}");
    }

    /// Bind the App's inbox to the session's own queue, so the input editor,
    /// the pending strip, and the worker's drive loop all read and write one
    /// inbox.  Called once at REPL start; before it, the App holds the throwaway
    /// inbox [`App::new`] seeded so `draw` has something to snapshot.
    pub fn bind_inbox(&mut self, inbox: Inbox) {
        self.inbox = inbox;
    }

    /// Bind the App's focus to the fleet's shared handle (the trunk's), so a
    /// `TAB` here and the focused agent's park predicate read and write one
    /// [`AtomicU64`].  Called once at REPL start, like [`Self::bind_inbox`].
    pub fn bind_focus(&mut self, focus: Arc<AtomicU64>) {
        self.focus = focus;
    }

    /// Walk up the `parents` chain from a (dying) agent to the nearest still-
    /// live ancestor tab, falling back to root — the focus target when a
    /// focused agent ends.
    fn parent_focus(&self, id: AgentId) -> AgentId {
        let mut cur = id;
        while let Some(&p) = self.parents.get(&cur) {
            if self.tabs.contains(&p) {
                return p;
            }
            cur = p;
        }
        self.root
    }

    /// Mutable access to the active picker, for the REPL's picker loop.
    fn picker_mut(&mut self) -> Option<&mut Picker> {
        self.picker.as_mut()
    }

    pub fn busy_off(&mut self) {
        // A turn ending supersedes any live phase label: clear it on the
        // focused viewport so the elapsed-wait bar disappears.
        let focused = self.focused();
        if let Some(vp) = self.viewports.get_mut(&focused) {
            vp.clear_phase();
        }
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
    pub fn clear(&mut self, info: &SessionInfo<'_>, term: &mut Term) -> io::Result<()> {
        let root = self.root;
        // Retire every still-live non-root tab into the linger window. A tab
        // already dying keeps its earlier death instant, so a child that died
        // just before the clear is not given a fresh full window.
        let now = Instant::now();
        let retiring: Vec<AgentId> = self.tabs.iter().copied().filter(|&id| id != root).collect();
        for id in retiring {
            self.dying.entry(id).or_insert(now);
        }
        self.focus.store(root, Ordering::Relaxed);
        // A `/clear` on the trunk cancels an in-flight model response in
        // `route_submit`; the cancel trips within one `wait_for_cancel` poll
        // (~50 ms), but the unbounded bus can still carry tokens the worker
        // emitted before the streaming select noticed the flag.  Until the
        // next prompt echoes genuinely, drop those stragglers in
        // [`Self::handle`].
        self.root_clear_drain = true;
        if let Some(vp) = self.viewports.get_mut(&root) {
            vp.reset();
        }
        self.total_usage = Usage::default();
        self.last_input = 0;
        self.patch_buf = None;
        self.io_buf = None;
        self.selection = None;
        self.press = None;
        // A fresh root: drop queued user prompts and any stale non-human
        // deliveries (a wakeup or agent result that has not been drained).
        self.inbox.clear();
        self.banner(term, info)
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
        if self.dying.contains_key(&id) {
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
        if id == self.root && self.root_clear_drain {
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
            && let Some(vp) = self.viewports.get_mut(&id)
        {
            vp.clear_phase();
        }
        match kind {
            Kind::Born {
                log_dir,
                title,
                parent,
            } => {
                if let std::collections::hash_map::Entry::Vacant(slot) = self.viewports.entry(id) {
                    let agent = AgentSlot((self.tabs.len() as u8) % AGENT_HUES.len() as u8);
                    slot.insert(Viewport::new(log_dir.join("user.log"), agent));
                    self.dispatch_order.push(id);
                }
                self.titles.insert(id, title);
                // Record the spawn edge so focus can fall back to the parent
                // when this agent ends while focused.
                self.parents.insert(id, parent);
                if !self.tabs.contains(&id) {
                    self.tabs.push(id);
                }
            }
            Kind::Died => {
                self.flush_surfaces();
                let floor = self.context_floor();
                if let Some(vp) = self.viewports.get_mut(&id) {
                    vp.flush_open(floor);
                }
                // Root never enters the linger window; it lives as
                // long as the program does.
                if id != self.root {
                    self.dying.insert(id, Instant::now());
                    // A `reply` ends an agent even while the human is focused on
                    // it; the conversation ends out from under them, so focus
                    // falls back to its parent, recursing toward the trunk.  A
                    // de-focused agent that reaps already moved focus on `TAB`,
                    // so this only fires for the focused one.
                    if self.focus.load(Ordering::Relaxed) == id {
                        self.focus.store(self.parent_focus(id), Ordering::Relaxed);
                    }
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
                if let Some(vp) = self.viewports.get_mut(&id) {
                    vp.add_usage(u);
                }
            }
            Kind::Token(text) => {
                self.flush_surfaces();
                let floor = self.context_floor();
                if let Some(vp) = self.viewports.get_mut(&id) {
                    vp.push_token(&text, floor);
                }
            }
            Kind::Boundary => {
                self.flush_surfaces();
                let floor = self.context_floor();
                if let Some(vp) = self.viewports.get_mut(&id) {
                    vp.close_boundary(floor);
                }
            }
            // Arrives after `Boundary` has flushed the answer prose into
            // blocks, so the turn's first prose block exists to wear the
            // shadow.
            Kind::Reasoning { text, answer_chars } => {
                self.with_viewport(id, |vp| vp.attach_reasoning(text, answer_chars));
            }
            Kind::Step(n) => self.push_chrome(id, RailShape::Step, line::step(n as usize)),
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
                    // A summary-less call is a query — coalesced on screen into
                    // one `tool : q1, q2, …` line.  Its cmd being the
                    // parse-failure sentinel makes it an invisible boundary
                    // (`None`): present only so its result attaches there, never
                    // reaching back to clobber an earlier call's size bar.
                    None => vp.push_query(tool, (cmd != crate::tools::INVALID_INPUT).then_some(cmd)),
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
            Kind::SystemNote(text) => self.push_chrome(id, RailShape::Plain, line::dim(&text)),
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
                let root = self.root;
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
                    self.viewports.keys().copied().collect::<Vec<_>>(),
                    self.focused(),
                    card.single_diff().is_some()
                );
                match card.into_single_diff() {
                    Ok((path, hunks)) => self.absorb_patch(id, path, hunks),
                    Err(card) => self.with_viewport(id, |vp| vp.push_card(card)),
                }
            }
            // A structural I/O effect core surfaced: a read, write, exec, or
            // grep.  Each lands as its own `Kind::Io`, so a burst reads as
            // `Read…, $…, Read…, $…` clutter — the io buffer collapses a run
            // (even interleaved) into one block per kind, flushed at the next
            // boundary.  The per-event `card` is dropped on the render path; it
            // is reconstructed grouped at flush, and the structured per-event
            // record already reached the transcript at the emit seam
            // (`Emitter::emit`), upstream of this UI handler, so nothing is lost.
            Kind::Io { event, .. } => self.absorb_io(id, event),
            // Pinned state: write or drop a register slot in place.  Routed
            // directly, *not* through `with_viewport` — a pin is ambient state
            // like `Kind::Usage`, never a scrollback barrier, so it must not
            // flush the io/patch grouping windows the way a landing block does.
            Kind::Pin { key, card } => {
                if let Some(vp) = self.viewports.get_mut(&id) {
                    vp.set_pin(key, card);
                }
            }
            Kind::Unpin { key } => {
                if let Some(vp) = self.viewports.get_mut(&id) {
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
        self.flush_surfaces();
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

    fn push_chrome(&mut self, id: AgentId, shape: RailShape, lines: Vec<Line<'static>>) {
        self.with_viewport(id, |vp| vp.push_chrome(shape, lines));
    }

    /// Draw a dim UI note straight to the viewport — view-local chrome (a slash
    /// legend row, a clipboard or export ack) that names nothing about the run,
    /// so it is *drawn, not recorded*: it never becomes an event, the way the
    /// rendered `Kind::SystemNote` does at the emit seam.
    fn push_note(&mut self, id: AgentId, text: String) {
        self.push_chrome(id, RailShape::Plain, line::dim(&text));
    }

    /// Draw an error line straight to the viewport — the UI-thread twin of
    /// [`Agent::note_error`], for the view commands that surface their own
    /// failures.  Drawn, not recorded.
    fn push_error(&mut self, id: AgentId, message: String) {
        self.push_chrome(id, RailShape::Error, line::error(&message));
    }

    /// Absorb a single-`diff` card's hunks into [`Self::patch_buf`], or
    /// flush + open a fresh buffer when the path or session changes.
    /// Consecutive same-`(id, path)` diff cards append their hunks into one
    /// buffer so they later render as a single `▎ diff <path>` block of
    /// located hunks — the way a unified diff presents several changes to
    /// one file.
    fn absorb_patch(&mut self, id: AgentId, path: String, hunks: Vec<Hunk>) {
        let same = self
            .patch_buf
            .as_ref()
            .is_some_and(|b| b.id == id && b.path == path);
        if same {
            let buf = self.patch_buf.as_mut().expect("same-path implies Some");
            buf.hunks.extend(hunks);
        } else {
            self.flush_patch_buf();
            self.patch_buf = Some(PatchBuf { id, path, hunks });
        }
    }

    /// Commit any pending [`PatchBuf`] as one `▎ diff` block.  Called at
    /// every commit boundary that isn't another single-`diff` card
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

    /// Bucket a structural I/O `event` into [`Self::io_buf`] by kind, deduped
    /// and order-independent (the user does not care about interleave order).
    /// A session change flushes the in-flight buffer and opens a fresh one, so
    /// a cross-session burst never merges two sessions' surfaces into one
    /// block.  Unlike [`Self::with_viewport`], this accumulates directly: the
    /// shared [`Self::flush_surfaces`] boundary is what would flush the very
    /// buffer being filled, so routing through it would defeat the grouping.
    fn absorb_io(&mut self, id: AgentId, event: IoEvent) {
        if self.io_buf.as_ref().is_some_and(|b| b.id != id) {
            self.flush_io_buf();
        }
        let buf = self.io_buf.get_or_insert_with(|| IoBuf {
            id,
            reads: Vec::new(),
            execs: Vec::new(),
            greps: Vec::new(),
            writes: Vec::new(),
        });
        match event {
            IoEvent::Read { path } => {
                if !buf.reads.contains(&path) {
                    buf.reads.push(path);
                }
            }
            IoEvent::Exec {
                argv,
                outcome,
                status,
            } => {
                let dup = buf
                    .execs
                    .iter()
                    .any(|e| matches!(e, IoEvent::Exec { argv: a, .. } if *a == argv));
                if !dup {
                    buf.execs.push(IoEvent::Exec {
                        argv,
                        outcome,
                        status,
                    });
                }
            }
            grep @ IoEvent::Grep { .. } => {
                if !buf.greps.contains(&grep) {
                    buf.greps.push(grep);
                }
            }
            IoEvent::Write {
                path,
                mode,
                outcome,
            } => {
                // Keep the latest outcome: a re-write of the same path replaces
                // the buffered entry rather than stacking a duplicate.
                let event = IoEvent::Write {
                    path: path.clone(),
                    mode,
                    outcome,
                };
                match buf
                    .writes
                    .iter_mut()
                    .find(|e| matches!(e, IoEvent::Write { path: p, .. } if *p == path))
                {
                    Some(slot) => *slot = event,
                    None => buf.writes.push(event),
                }
            }
        }
    }

    /// Commit any pending [`IoBuf`] as one block *per non-empty kind*, in a
    /// fixed Read → Exec → Grep → Write order, reusing the exact `io_card` span
    /// idioms via [`crate::card::io_group_card`].  No-op when the buffer is
    /// empty.  Called at every commit boundary that isn't another io surface
    /// in the same session, through the shared [`Self::flush_surfaces`].
    fn flush_io_buf(&mut self) {
        let Some(buf) = self.io_buf.take() else {
            return;
        };
        if let Some(vp) = self.viewports.get_mut(&buf.id) {
            // One block per non-empty kind, in the fixed Read → Exec → Grep →
            // Write order, each carrying its `IoKind` and the count it folds —
            // the run's census tally.  Reads / greps / execs are *observations*
            // the coalescing projection folds under their call; writes are
            // *barriers* that end the ral block.  Each per-kind group yields one
            // card (or none), reconstructed from the same `io_group_card` span
            // idioms.
            use crate::card::io_group_card;
            for card in io_group_card(&buf.reads, &[], &[], &[]) {
                vp.push_io_card(card, IoKind::Read, buf.reads.len() as u32);
            }
            for card in io_group_card(&[], &buf.execs, &[], &[]) {
                vp.push_io_card(card, IoKind::Exec, buf.execs.len() as u32);
            }
            for card in io_group_card(&[], &[], &buf.greps, &[]) {
                vp.push_io_card(card, IoKind::Grep, buf.greps.len() as u32);
            }
            for card in io_group_card(&[], &[], &[], &buf.writes) {
                vp.push_io_card(card, IoKind::Write, buf.writes.len() as u32);
            }
        }
    }

    /// The shared external commit boundary: flush both grouping buffers, io
    /// first so an io group lands before any diff that the same boundary
    /// commits.  Every non-io, non-diff surface funnels here (the
    /// [`Self::with_viewport`] chokepoint, plus session death, the streaming
    /// token, and the turn boundary), so the two separate buffers — keyed
    /// differently, never generalised into one — share only this boundary.
    fn flush_surfaces(&mut self) {
        self.flush_io_buf();
        self.flush_patch_buf();
    }

    /// Redraw the whole frame: the focused session's visible rows fill the
    /// content area's left, the pinned-state register column its right edge,
    /// with a blank breathing row, then the tab bar, status row, prompt, and
    /// footer pinned beneath.  The content geometry is stashed in
    /// [`Self::frame`] so the next mouse event maps to a buffer row.
    pub fn draw(&mut self, term: &mut Term) -> io::Result<()> {
        let (cols, rows) = size().unwrap_or((READ_W, 24));
        let area = Rect::new(0, 0, cols, rows);
        // The prompt box sizes to its draft; the `/model` picker floats as an
        // overlay above this whole layout (drawn last over a cleared centre),
        // so it no longer claims the prompt region.
        let prompt_h = prompt_height(&self.textarea, area.width, area.height);
        let tab_h = if self.tabs.len() > 1 {
            self.tabs.len() as u16
        } else {
            0u16
        };
        // The pending-prompt strip above the input: messages the user queued
        // mid-turn, waiting for the next tool-result or turn boundary. Its
        // width matches the content column (capped at READ_W like the
        // transcript), and its height is capped at a third of the screen so a
        // long queue can never crowd the transcript off-screen.
        let queued = self.inbox.snapshot();
        let queued_lines = if queued.is_empty() {
            Vec::new()
        } else {
            let w = area.width.saturating_sub(LEFT_MARGIN).min(READ_W);
            line::queued_prompt(&queued, w, (area.height / 3).max(1) as usize)
        };
        let queued_h = queued_lines.len() as u16;
        // The register's vertical budget is decided here, before the layout:
        // shown as the right-hand column when the focused session has pins and
        // the terminal is wide enough to spare the margin, else collapsed to a
        // one-row pin band beside the matrix.  `content.width == area.width`
        // (the vertical split keeps full width), so the threshold reads off
        // `area.width` directly.
        let focused = self.focused();
        let has_pins = self
            .viewports
            .get(&focused)
            .is_some_and(|vp| !vp.pins().is_empty());
        let show_register =
            has_pins && area.width >= LEFT_MARGIN + READ_W + REGISTER_GAP + REGISTER_W;
        let pin_band_h = (has_pins && !show_register) as u16;
        let layout = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1), // breathing row between output and chrome
            Constraint::Length(tab_h),
            Constraint::Length(pin_band_h), // collapsed register, beside the matrix
            Constraint::Length(queued_h),
            Constraint::Length(prompt_h),
            Constraint::Length(1), // rule_line: sits below prompt, above footer
            Constraint::Length(1),
        ])
        .split(area);
        let (content, tab_row, pin_band_row, queued_row, prompt_row, status_row, footer_row) = (
            layout[0], layout[2], layout[3], layout[4], layout[5], layout[6], layout[7],
        );
        // Split the content row by hand into the rail's left gutter, the
        // transcript, and — on a wide enough terminal — the register glued to
        // the right edge.  No scrollbar: the right edge is the register's, and
        // scroll position reads as a magnitude in the rule line.  Capping the
        // transcript at READ_W (rather than letting it expand) is what keeps
        // the register from ever narrowing prose: it claims only dead margin.
        let text_w = if show_register {
            READ_W
        } else {
            content.width.saturating_sub(LEFT_MARGIN)
        };
        let text_rect = Rect::new(content.x + LEFT_MARGIN, content.y, text_w, content.height);
        let register_rect = show_register.then(|| {
            Rect::new(
                content.x + content.width - REGISTER_W,
                content.y,
                REGISTER_W,
                content.height,
            )
        });
        // Inset the queued-prompt strip, pin band, and rule line to share the
        // transcript's left gutter.
        let queued_rect = Rect::new(
            queued_row.x + LEFT_MARGIN,
            queued_row.y,
            queued_row.width.saturating_sub(LEFT_MARGIN),
            queued_row.height,
        );
        let pin_band_rect = Rect::new(
            pin_band_row.x + LEFT_MARGIN,
            pin_band_row.y,
            pin_band_row.width.saturating_sub(LEFT_MARGIN),
            pin_band_row.height,
        );
        let status_rect = Rect::new(
            status_row.x + LEFT_MARGIN,
            status_row.y,
            status_row.width.saturating_sub(LEFT_MARGIN),
            status_row.height,
        );
        // Pre-render the register's content (the focused session's pins, in its
        // agent hue) before the borrow needed by `render_window`: as the full
        // right column when shown, else as the collapsed one-row band.
        let (register_lines, pin_band_lines): (Vec<Line<'static>>, Vec<Line<'static>>) =
            match self.viewports.get(&focused) {
                Some(vp) if show_register => {
                    let hue = AGENT_HUES
                        .get(vp.agent().0 as usize)
                        .copied()
                        .unwrap_or(AGENT_HUES[0]);
                    (
                        line::render_register(vp.pins(), REGISTER_W, hue),
                        Vec::new(),
                    )
                }
                Some(vp) if pin_band_h > 0 => (Vec::new(), line::pin_band(vp.pins())),
                _ => (Vec::new(), Vec::new()),
            };
        let (mut lines, offset, scroll_pct) = match self.viewports.get_mut(&focused) {
            Some(vp) => {
                let w = vp.render_window(text_rect.width, text_rect.height as usize);
                (w.lines, w.offset, w.scroll_pct)
            }
            None => (Vec::new(), 0, None),
        };
        self.paint_selection(&mut lines, offset);
        self.paint_hover(&mut lines, offset);
        self.frame = Some(FrameGeom {
            text: text_rect,
            offset,
        });

        self.style_prompt();
        // The rule_line reads the focused viewport's live phase: its label
        // (cloned to outlive the borrow) and its elapsed wall-time, which the
        // elapsed-wait bar encodes.
        let (phase, wait_elapsed) = self
            .viewports
            .get(&focused)
            .map(|vp| (vp.phase_label().map(str::to_owned), vp.phase_elapsed()))
            .unwrap_or_default();
        let usage = self.total_usage;
        let last_input = self.last_input;
        let context_window = self.context_window;
        let status_model = self.status_model.clone();
        // The matrix replaces the tab bar when more than one session is
        // live: one row per agent, each owning its `Line` so the `'static`
        // draw closure captures no borrow of `self`.
        let matrix_lines = (self.tabs.len() > 1).then(|| {
            let rows: Vec<(AgentId, &Viewport)> = self
                .tabs
                .iter()
                .filter_map(|&id| self.viewports.get(&id).map(|vp| (id, vp)))
                .collect();
            matrix_bar(&rows, &self.titles, focused, &self.dying, self.matrix_sort)
        });
        let prompt_hint = self.prompt_hint(focused);
        let picker = self.picker.as_ref();

        // Bracket the frame's terminal writes in a synchronized update so the
        // emulator buffers the whole diff and swaps it atomically.  Without
        // this, a tail-following redraw rewrites every visible cell each tick,
        // and a terminal scanning the screen mid-write shows a half-painted
        // frame — the tearing seen when a full page streams tool calls.
        // `End` is emitted on the error path too, so a failed draw never
        // strands the terminal in synchronized mode.
        execute!(io::stdout(), BeginSynchronizedUpdate)?;
        let drawn = term.draw(|f| {
            f.render_widget(Paragraph::new(lines), text_rect);
            // The register column (or its collapsed band) — the focused
            // session's pinned state, painted in place on the right edge.
            if let Some(reg) = register_rect {
                f.render_widget(Paragraph::new(register_lines), reg);
            } else if !pin_band_lines.is_empty() {
                f.render_widget(Paragraph::new(pin_band_lines), pin_band_rect);
            }
            if let Some(matrix) = matrix_lines {
                f.render_widget(Paragraph::new(matrix), tab_row);
            }
            if !queued_lines.is_empty() {
                f.render_widget(Paragraph::new(queued_lines), queued_rect);
            }
            f.render_widget(
                Paragraph::new(rule_line(
                    text_rect.width.min(READ_W) as usize,
                    phase.as_deref(),
                    wait_elapsed,
                    scroll_pct,
                    StatusReadout {
                        usage: &usage,
                        last_input,
                        context_window,
                        model: &status_model,
                    },
                )),
                status_rect,
            );
            // The prompt region draws normally; the `/model` picker, when
            // open, floats over the whole frame below (its own [`Clear`]ed
            // centre), so it never displaces the input. Input lives in main
            // only; a subagent tab shows a watch-only hint in the prompt slot,
            // and the textarea keeps its draft for when the user tabs home.
            match prompt_hint {
                Some(line) => {
                    let block = ratatui::widgets::Block::default()
                        .borders(ratatui::widgets::Borders::ALL)
                        .border_type(ratatui::widgets::BorderType::Rounded)
                        .border_style(Style::default().fg(SLATE).add_modifier(Modifier::DIM))
                        .padding(ratatui::widgets::Padding::horizontal(PROMPT_PAD_H));
                    f.render_widget(Paragraph::new(line).block(block), prompt_row);
                }
                None => {
                    f.render_widget(&self.textarea, prompt_row);
                    // Show the terminal's native cursor at the edit point —
                    // but not while the picker overlay owns the keyboard, or
                    // the cursor would peek out beneath the modal.
                    if picker.is_none() {
                        let inner = ratatui::widgets::Block::default()
                            .borders(ratatui::widgets::Borders::ALL)
                            .padding(ratatui::widgets::Padding::horizontal(PROMPT_PAD_H))
                            .inner(prompt_row);
                        place_native_cursor(f, inner, &self.textarea);
                    }
                }
            }
            f.render_widget(Paragraph::new(footer_hint()), footer_row);
            // Last: the floating picker, over the dimmed session.
            if let Some(p) = picker {
                p.render(f, area);
            }
        });
        execute!(io::stdout(), EndSynchronizedUpdate)?;
        drawn?;
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

    /// Brighten the rail glyph of the hovered dialable block so the dial
    /// target reads as a lit button under the pointer.  Only the block's
    /// first visible row carries the rail glyph (body rows have none), so
    /// the reverse lands on the leading span of that row alone; a block
    /// whose header has scrolled off the top shows no mark until it
    /// returns into view.
    fn paint_hover(&self, lines: &mut [Line<'static>], offset: usize) {
        let Some(target) = self.hover else {
            return;
        };
        let Some(vp) = self.viewports.get(&self.focused()) else {
            return;
        };
        for (i, line) in lines.iter_mut().enumerate() {
            let row = offset + i;
            let head = row == 0 || vp.block_at(row - 1) != Some(target);
            if vp.block_at(row) == Some(target) && head {
                if let Some(glyph) = line.spans.first_mut() {
                    glyph.style = glyph.style.add_modifier(Modifier::REVERSED);
                }
                break;
            }
        }
    }

    /// The watch-only banner shown in the prompt slot on a subagent tab,
    /// or `None` on main where the textarea is editable.
    fn prompt_hint(&self, focused: AgentId) -> Option<Line<'static>> {
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

    /// Currently focused tab.  Resolves a stale focus (a subagent that aged
    /// out of the tab bar, or the no-focus sentinel) to the root.
    pub(in crate::tui) fn focused(&self) -> AgentId {
        let f = self.focus.load(Ordering::Relaxed);
        if self.tabs.contains(&f) { f } else { self.root }
    }

    /// Expire `dying` entries that have outlived [`LINGER`].  Called
    /// once per frame from the event loop.  When the focused tab
    /// expires, focus falls back to its parent (recursing toward the trunk).
    pub fn tick(&mut self) {
        let now = Instant::now();
        let expired: Vec<AgentId> = self
            .dying
            .iter()
            .filter(|&(_, &t)| now.duration_since(t) >= LINGER)
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            self.dying.remove(&id);
            self.tabs.retain(|&t| t != id);
            self.titles.remove(&id);
            if self.focus.load(Ordering::Relaxed) == id {
                let fallback = self.parent_focus(id);
                self.focus.store(fallback, Ordering::Relaxed);
            }
            self.parents.remove(&id);
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

    /// Pull every pending prompt back into the editor for revision, joined
    /// with a blank line so each queued message stays distinct.  A non-empty
    /// live draft wins over queue editing: Up keeps its ordinary history
    /// behaviour rather than discarding text the user has started.
    fn edit_queued_prompt(&mut self) -> bool {
        if self.hist_pos.is_some() || self.textarea.lines().iter().any(|line| !line.is_empty()) {
            return false;
        }
        let Some(prompts) = self.inbox.pop_back_user_all() else {
            return false;
        };
        let joined = prompts.join("\n\n");
        self.set_prompt(&joined);
        true
    }

    pub fn paste(&mut self, s: &str) {
        self.cx_pending = false;
        self.textarea.insert_str(s);
    }

    /// Adopt `text` returned by the external editor as the live prompt draft,
    /// leaving any in-progress history browse so a later Up/Down does not
    /// overwrite the edit.
    fn adopt_draft(&mut self, text: &str) {
        self.hist_pos = None;
        self.set_prompt(text);
    }

    /// Take the pending `C-x C-e` request, if any: the UI loop calls this after
    /// each edit key to learn whether it must suspend for the external editor.
    fn take_editor_request(&mut self) -> bool {
        std::mem::take(&mut self.editor_request)
    }

    /// The prompt's current contents, lines newline-joined.
    fn prompt_text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Recolor the prompt text in place: a line that names a known slash
    /// command (so the UI loop will dispatch it) glows cyan and bold; anything
    /// else stays plain white. Driven once per frame from [`App::draw`], so it
    /// tracks every edit — typing, paste, history recall.
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
        if std::mem::take(&mut self.cx_pending) {
            if can_edit
                && k.code == KeyCode::Char('e')
                && k.modifiers.contains(KeyModifiers::CONTROL)
            {
                self.editor_request = true;
            }
            return;
        }
        if can_edit && k.code == KeyCode::Char('x') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.cx_pending = true;
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
            KeyCode::PageUp => self.scroll_page(-1),
            KeyCode::PageDown => self.scroll_page(1),
            // Not collapsible into a match guard: with <=1 tab, Tab must
            // be a no-op, not fall through to the textarea-input arm below.
            #[allow(clippy::collapsible_match)]
            KeyCode::Tab => {
                if self.tabs.len() > 1 {
                    let current = self.focused();
                    if let Some(pos) = self.tabs.iter().position(|&id| id == current) {
                        let next = self.tabs[(pos + 1) % self.tabs.len()];
                        self.focus.store(next, Ordering::Relaxed);
                    }
                }
            }
            // Shift+Tab toggles the matrix row order between spawn and
            // cost — a render-time projection that surfaces the
            // budget-burner; inert with a single session (no matrix).
            KeyCode::BackTab if self.tabs.len() > 1 => {
                self.matrix_sort = match self.matrix_sort {
                    MatrixSort::Spawn => MatrixSort::Cost,
                    MatrixSort::Cost => MatrixSort::Spawn,
                };
            }
            // Up/Down walk the prompt history, but only from the
            // prompt's edge rows: with the cursor mid-text in a
            // multi-line draft they fall through and move the cursor.
            // When the prompt is empty and prompts are queued above it,
            // Up pulls the entire queued run back down into the editor,
            // dequeueing all of them so the user can revise the whole batch.
            KeyCode::Up if self.focused() == self.root && k.modifiers.is_empty() => {
                if self.textarea.cursor().0 == 0 {
                    if !self.edit_queued_prompt() {
                        self.history_prev();
                    }
                } else {
                    self.edit_input(k);
                }
            }
            KeyCode::Down if self.focused() == self.root && k.modifiers.is_empty() => {
                let last_row = self.textarea.lines().len() - 1;
                if self.textarea.cursor().0 == last_row {
                    self.history_next();
                } else {
                    self.edit_input(k);
                }
            }
            _ if can_edit => {
                self.edit_input(k);
            }
            _ => {}
        }
    }

    /// Route a plain text-input key into the editable prompt — the single
    /// dispatch point for the three [`Self::key`] arms that previously each
    /// called `self.textarea.input(k)` directly.  In the default emacs mode
    /// (`self.vim == None`) it is byte-for-byte that call.  In vi mode it folds
    /// the key through the shared [`textarea_vim::Vim::advance`] driver, which
    /// re-styles the cursor on a mode change and treats `Quit` as a no-op (a
    /// REPL prompt has no editor to quit).
    fn edit_input(&mut self, k: KeyEvent) {
        match self.vim.take() {
            None => {
                self.textarea.input(k);
            }
            Some(vim) => self.vim = Some(vim.advance(k.into(), &mut self.textarea)),
        }
    }

    /// Route a mouse event: the wheel scrolls, a left-drag selects (and
    /// copies on release), and a left click that never dragged opens the
    /// block it landed on.  Shift+left falls through to the terminal's
    /// own selection, so we never see — or fight — it.
    pub fn mouse(&mut self, me: MouseEvent) {
        self.cx_pending = false;
        // Refresh the hover mark on every event — motion, wheel, or press —
        // so the brightened dial glyph tracks the pointer the instant it
        // crosses a dialable block.
        self.hover = self.hover_block(me);
        match me.kind {
            // Over the rail glyph of a dialable block, the wheel dials the
            // block's disclosure level (up reveals, down reduces) and
            // consumes the event; otherwise it scrolls the viewport.
            MouseEventKind::ScrollUp if self.rail_dial(me, 1) => {}
            MouseEventKind::ScrollDown if self.rail_dial(me, -1) => {}
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

    /// Map a wheel event over a block's rail to its buffer row's block, or
    /// `None` when the event falls outside the rail columns (cols 0–1 of
    /// the content rect — the [`RAIL_W`] target the click-cycle shares) or
    /// past the buffer.  Both columns dial, so the wheel claims the same
    /// two-wide strip the click does rather than a single-glyph needle.
    fn rail_block(&self, me: MouseEvent) -> Option<usize> {
        let frame = self.frame?;
        let on_rail = me.column >= frame.text.x
            && (me.column as usize) < frame.text.x as usize + RAIL_W
            && me.row >= frame.text.y
            && me.row < frame.text.y + frame.text.height;
        if !on_rail {
            return None;
        }
        let row = frame.offset + (me.row - frame.text.y) as usize;
        self.viewports.get(&self.focused())?.block_at(row)
    }

    /// The dialable block under the pointer anywhere in the content rect,
    /// or `None` over inert chrome, a non-dialable block, or past the
    /// buffer.  Wider than [`Self::rail_block`]: the whole block row claims
    /// the hover, so the dial glyph lights the moment the pointer reaches
    /// the block and guides it to the rail rather than waiting for a hit.
    fn hover_block(&self, me: MouseEvent) -> Option<usize> {
        let frame = self.frame?;
        if !contains(frame.text, me.column, me.row) {
            return None;
        }
        let row = frame.offset + (me.row - frame.text.y) as usize;
        let vp = self.viewports.get(&self.focused())?;
        let idx = vp.block_at(row)?;
        vp.block_dialable(idx).then_some(idx)
    }

    /// Dial the block under a rail-glyph wheel event by `delta`,
    /// returning whether the event was consumed — `true` whenever it sat
    /// on the glyph of a dialable block, so a wheel that overshoots the
    /// clamp rests as an inert no-op rather than spilling into a page
    /// scroll.  Only a glyph over inert chrome leaves the wheel to scroll.
    fn rail_dial(&mut self, me: MouseEvent, delta: i8) -> bool {
        let Some(idx) = self.rail_block(me) else {
            return false;
        };
        let id = self.focused();
        let Some(vp) = self.viewports.get_mut(&id) else {
            return false;
        };
        if !vp.block_dialable(idx) {
            return false;
        }
        vp.dial_block(idx, delta);
        true
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
        let on_rail = (me.column as usize) < frame.text.x as usize + RAIL_W;
        self.press = Some(Press {
            row,
            block,
            on_rail,
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
    /// click on the rail cycles the block it landed on (L1↔L3); a click
    /// off the rail stays selection.
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
        } else if press.on_rail
            && let Some(idx) = press.block
        {
            if let Some(vp) = self.viewports.get_mut(&id) {
                vp.cycle_block(idx);
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
        let floor = self.context_floor();
        for vp in self.viewports.values_mut() {
            vp.flush_open(floor);
        }
        let mut paths = Vec::with_capacity(self.dispatch_order.len());
        for &id in &self.dispatch_order {
            if let Some(vp) = self.viewports.get_mut(&id) {
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
        self.viewports
            .get(&self.focused())
            .map(Viewport::latest_reply_md)
            .unwrap_or_default()
    }

    /// Flush the focused tab's `user.log` and return its path, so `/export`
    /// can copy the rendered transcript elsewhere.  Flushes the open
    /// markdown buffer first, mirroring [`Self::flush_logs`], so a trailing
    /// streamed paragraph reaches the file before the copy.
    pub(in crate::tui) fn flush_focused_log(&mut self) -> io::Result<PathBuf> {
        let focused = self.focused();
        let floor = self.context_floor();
        let vp = self
            .viewports
            .get_mut(&focused)
            .expect("focused tab always has a viewport");
        vp.flush_open(floor);
        Ok(vp.flush_log()?.to_path_buf())
    }

    pub fn banner(&mut self, term: &mut Term, s: &SessionInfo<'_>) -> io::Result<()> {
        // The wordmark + eagle: a branded splash, an image outside Bertin's
        // data variables, so it alone keeps the saturated palette and reads
        // as neon. It carries no rail — it is not a row on the plane.
        let mut splash: Vec<Line<'static>> = vec![Line::default()];
        for (a, e) in ART.lines().zip(EAGLE.lines()) {
            splash.push(Line::from(vec![
                bold(a.to_string(), BANNER_PINK),
                Span::raw("  "),
                bold(e.to_string(), BANNER_GOLD),
            ]));
        }

        if let Some(vp) = self.viewports.get_mut(&self.root) {
            vp.push_chrome(RailShape::Plain, splash);
            vp.push_chrome(RailShape::Plain, line::render_card(&session_card(s), 3));
        }
        self.draw(term)
    }
}

/// The startup session metadata as a Bertin matrix — one `fields` mark the
/// banner pushes through the shared aligned-column renderer, so it reads in
/// the muted palette like every other block.  It is ambient startup chrome,
/// rail-less Plain like the splash above it.  Hue is spent only where it
/// names something: a path carries the Path identity, a `dangerous` base
/// alarms; names and quantities stay plain ink.
fn session_card(s: &SessionInfo<'_>) -> Card {
    let mut rows: Vec<Field> = vec![
        meta_field("cwd", vec![meta_span(Role::Path, s.cwd)]),
        meta_field("provider", vec![meta_span(Role::Strong, s.provider)]),
    ];

    let mut model_val = vec![meta_span(Role::Strong, s.model)];
    if let Some(slug) = s.canonical_slug
        && slug != s.model
    {
        model_val.push(meta_span(Role::Muted, format!(" ({slug})")));
    }
    rows.push(meta_field("model", model_val));

    if let Some(ctx) = s.context_window {
        rows.push(meta_field(
            "context",
            vec![meta_plain(provider::humanize_tokens(ctx))],
        ));
    }

    let max_t = match (s.max_tokens_override, s.max_output_tokens) {
        (Some(n), _) => n.to_string(),
        (None, Some(catalog)) => {
            format!("auto (≤{})", provider::humanize_tokens(catalog as u64))
        }
        (None, None) => "auto".into(),
    };
    rows.push(meta_field("max-tokens", vec![meta_plain(max_t)]));

    let base_role = if s.base == "dangerous" {
        Role::Bad
    } else {
        Role::Strong
    };
    rows.push(meta_field("base", vec![meta_span(base_role, s.base)]));

    rows.push(meta_field(
        "extend-base",
        match s.extend_base {
            Some(p) => vec![meta_span(Role::Path, p.display().to_string())],
            None => vec![meta_span(Role::Muted, "none")],
        },
    ));

    rows.push(meta_field(
        "restrict",
        if s.restrict_files.is_empty() {
            vec![meta_span(Role::Muted, "none")]
        } else {
            vec![meta_span(Role::Path, join_paths(s.restrict_files))]
        },
    ));

    let sz = format!("{:.1} kB", s.system_size as f64 / 1024.0);
    let mut sys_val = vec![meta_plain(sz), meta_span(Role::Muted, " · ")];
    if s.system_files.is_empty() {
        sys_val.push(meta_span(Role::Muted, "default"));
    } else {
        sys_val.push(meta_span(Role::Path, join_paths(s.system_files)));
    }
    rows.push(meta_field("system prompt", sys_val));

    rows.push(meta_field(
        "scratch",
        vec![meta_span(Role::Path, s.scratch.display().to_string())],
    ));

    Card(vec![Mark::Fields { rows }])
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

/// A roled value span for the startup metadata matrix — names a nominal
/// [`Role`] (the renderer binds it to a hue), never a colour.
fn meta_span(role: Role, text: impl Into<String>) -> CardSpan {
    CardSpan {
        role: Some(role),
        text: text.into(),
    }
}

/// A roleless value span — a quantity readout the matrix renders as plain
/// ink, carrying no nominal identity.
fn meta_plain(text: impl Into<String>) -> CardSpan {
    CardSpan {
        role: None,
        text: text.into(),
    }
}

/// One `(label, value)` row of the startup metadata matrix.
fn meta_field(label: &str, value: Vec<CardSpan>) -> Field {
    Field {
        label: label.to_string(),
        value: FieldVal::Inline(value),
    }
}

/// A comma-joined display of `paths` for a single matrix value cell.
fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The `/legend` panel: the transcript's own visual vocabulary, rendered
/// *as the graphic itself* — every sample is the literal output of the
/// builder that draws it on a real block, so the legend can never drift
/// from what the rail and bars actually paint.  It is ambient reference
/// chrome, rail-less Plain like the splash and [`session_card`]: a panel
/// to decode the transcript by, not a transcript event of its own.
///
/// The aligned `(label, sample)` rows go through [`line::legend_rows`] —
/// the same selective-alignment primitive the startup matrix uses — so the
/// samples land in one column; section titles between the row groups are
/// plain slate-bold heads.  The samples derive from [`rail::RAIL_SHAPES`],
/// [`AGENT_HUES`], and the bar / grain / spark / fidelity builders, so a
/// palette or shape change updates the legend with no edit here.
fn legend_panel() -> Vec<Line<'static>> {
    let head = |s: &str| {
        Line::from(Span::styled(
            s.to_string(),
            Style::default().fg(SLATE).add_modifier(Modifier::BOLD),
        ))
    };
    let note = |s: &str| Span::styled(s.to_string(), Style::default().fg(SLATE));

    let mut ls: Vec<Line<'static>> = vec![
        Line::default(),
        head("legend — the transcript as a graphic"),
    ];

    // ── rail: one cell, three variables ───────────────────────────────
    ls.push(Line::default());
    ls.push(head("rail · shape = block kind"));
    ls.extend(line::legend_rows(
        rail::RAIL_SHAPES
            .iter()
            .map(|(kind, name)| (*name, vec![rail::span(*kind, AgentSlot(0), None)]))
            .collect(),
    ));
    ls.push(Line::default());
    ls.push(head("rail · hue = which agent (constant down a tab)"));
    ls.extend(line::legend_rows(
        (0..AGENT_HUES.len())
            .map(|slot| {
                let label = if slot == 0 { "root" } else { "subagent" };
                (
                    label,
                    vec![rail::span(
                        RailKind::ToolCall(false),
                        AgentSlot(slot as u8),
                        None,
                    )],
                )
            })
            .collect(),
    ));
    ls.push(Line::default());
    ls.push(head("rail · value = magnitude (brighter is bigger)"));
    // One shape, the same hue, stepped up the value ramp by feeding the
    // four magnitude buckets `rail::value_step` reads — so the row *is* the
    // ramp the rail lightens by, not a restatement of it.
    ls.extend(line::legend_rows(vec![(
        "small → large",
        [None, Some(4), Some(20), Some(80)]
            .into_iter()
            .map(|mag| rail::span(RailKind::Patch, AgentSlot(0), mag))
            .collect(),
    )]));

    // ── strata: who is speaking, read off the background ───────────────
    ls.push(Line::default());
    ls.push(head("strata · background = machine region"));
    // Each swatch is the literal `line::wash` output, so the legend wears the
    // exact tones the transcript paints. Background is reserved for one thing —
    // machine text, a recessed panel; prose sits at the base, and your prompt
    // is fenced by a rule (the rail's `❖`), not a fill.
    let swatch = |text: &str, bg: Option<Color>| match bg {
        Some(bg) => line::wash(Line::from(Span::raw(text.to_string())), bg, None).spans,
        None => vec![note(text)],
    };
    ls.extend(line::legend_rows(vec![
        (
            "code",
            swatch(
                "scripts and shell output — a recessed panel",
                Some(line::CODE_BG),
            ),
        ),
        (
            "prose",
            swatch("model narration and replies — the base", None),
        ),
    ]));

    // ── the ordered bars ───────────────────────────────────────────────
    ls.push(Line::default());
    ls.push(head("bars · length and texture, beside a collapsed header"));
    ls.extend(line::legend_rows(vec![
        (
            "size",
            vec![line::size_bar(120), note("  log-scaled magnitude")],
        ),
        (
            "grain",
            vec![
                line::grain_run(9, 1),
                note("  diff density: ⣿ all adds → ⣀ all deletes"),
            ],
        ),
        (
            "sparkline",
            vec![
                Span::styled(
                    [None, Some(2), Some(40), Some(8), Some(300)]
                        .into_iter()
                        .map(line::spark_glyph)
                        .collect::<String>(),
                    Style::default().fg(SLATE),
                ),
                note("  one bar per call in a coalesced ral block"),
            ],
        ),
    ]));

    // ── the status line's two bottom bars ──────────────────────────────
    ls.push(Line::default());
    ls.push(head("status line · the two bars under the transcript"));
    ls.extend(line::legend_rows(vec![
        ("window", {
            let mut v = ctx_ramp(72, CTX_BAR_W);
            v.push(note("fills and brightens toward a full context window"));
            v
        }),
        ("elapsed", {
            let mut v = wait_bar(Duration::from_secs(18));
            v.push(note("grows with the current phase's wall-time"));
            v
        }),
    ]));

    // ── coherent degradation: how much to trust a passage ──────────────
    ls.push(Line::default());
    ls.push(head(
        "fidelity · a shaky answer renders drained, not authoritative",
    ));
    // Real prose through the real `render_md`, so the drain and wash are
    // exactly what a degraded block wears — never a re-derived colour.
    let prose = "An answer the model committed to the transcript.";
    let sample = |f: Fidelity| {
        md::render_md(prose, READ_W, 0, f)
            .into_iter()
            .next()
            .map(|line| line.spans)
            .unwrap_or_default()
    };
    ls.extend(line::legend_rows(vec![
        ("sound", sample(Fidelity::default())),
        (
            "drained",
            sample(Fidelity {
                context: 2,
                echo: 0,
            }),
        ),
        (
            "echoed",
            sample(Fidelity {
                context: 0,
                echo: 2,
            }),
        ),
    ]));
    ls.push(Line::from(note(
        "  context pressure drains the ink; echoing its own script washes the field behind it",
    )));

    // ── disclosure: detail is something you dial ───────────────────────
    ls.push(Line::default());
    ls.push(head("disclosure · dial detail on the rail (wheel / click)"));
    ls.push(Line::from(note(
        "  levels L1–L3; tool calls, diffs, and subagents floor at L1 — model prose always renders full",
    )));

    ls
}

/// The rule line's right-side status readout: model name, the ctx%
/// value-ramp inputs (`last_input` against `context_window`), and the
/// running token `usage` figures.
struct StatusReadout<'a> {
    usage: &'a Usage,
    last_input: u64,
    context_window: Option<u64>,
    model: &'a str,
}

fn rule_line(
    width: usize,
    phase: Option<&str>,
    wait_elapsed: Option<Duration>,
    scroll_pct: Option<u16>,
    status: StatusReadout<'_>,
) -> Line<'static> {
    let StatusReadout {
        usage,
        last_input,
        context_window,
        model: status_model,
    } = status;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut left_w = 0usize;

    // ── elapsed-wait bar ──────────────────────────────────────────────
    // A single bar that grows with the current phase's elapsed wall-time
    // and resets when the next phase starts. Size and value both encode
    // elapsed (see `wait_bar`): a snappy phase is a short dim stub, a
    // dragging one a long bright bar — so the row differs turn to turn and
    // the exception flares rather than the constant baseline. The `Ns`
    // digit ticks once per second: a calm, unmistakable liveness signal,
    // and the bar ceasing to grow means the turn has wedged.
    if let Some(elapsed) = wait_elapsed {
        let bar = wait_bar(elapsed);
        left_w += bar.iter().map(|s| s.width()).sum::<usize>();
        spans.extend(bar);
    }
    if let Some(p) = phase {
        let label = Span::styled(format!("{p}… "), Style::default().fg(SLATE));
        left_w += label.width();
        spans.push(label);
    }

    // ── status model ──────────────────────────────────────────────────
    if !status_model.is_empty() {
        let segment: Vec<Span<'static>> = vec![
            Span::styled(status_model.to_string(), Style::default().fg(SLATE)),
            Span::styled(" · ", Style::default().fg(SLATE)),
        ];
        left_w += segment.iter().map(|s| s.width()).sum::<usize>();
        spans.extend(segment);
    }

    // ── ctx% value-ramp ───────────────────────────────────────────────
    // A fixed-width lightness ramp: filled cells step toward white as
    // `last_input / context_window` approaches 1.0, empty cells dim
    // slate.  The eye reads the fill level and notices the approach to
    // full; the `N%` digit stays as a precise readout after the bar.
    // `context_window = None` → no ctx segment at all (as today).
    if let Some(cap) = context_window
        && cap > 0
    {
        let pct = ((last_input as f64 / cap as f64) * 100.0).round() as u64;
        let pct = pct.min(999);
        let bar = ctx_ramp(pct, CTX_BAR_W);
        left_w += bar.iter().map(|s| s.width()).sum::<usize>();
        spans.extend(bar);
        let readout = Span::styled(format!(" {pct}% "), Style::default().fg(SLATE));
        left_w += readout.width();
        spans.push(readout);
    }

    // ── scroll position ───────────────────────────────────────────────
    // Where the window sits in the scrollback, as a fixed-position value —
    // the deleted right-margin scrollbar's datum, re-encoded as a magnitude
    // the doctrine permits.  `⇣ bot` at the tail, `⇣ N%` above it; absent
    // when the whole buffer fits.
    if let Some(pct) = scroll_pct {
        let text = if pct >= 100 {
            "⇣ bot ".to_string()
        } else {
            format!("⇣ {pct}% ")
        };
        let seg = Span::styled(text, Style::default().fg(SLATE));
        left_w += seg.width();
        spans.push(seg);
    }

    // ── usage (right-aligned) ─────────────────────────────────────────
    let right = usage_text(usage);
    let rw: usize = right.iter().map(|s: &Span<'_>| s.width()).sum();
    let gap = width.saturating_sub(left_w + rw);
    if gap > 0 {
        spans.push(Span::styled(" ".repeat(gap), Style::default().fg(SLATE)));
    }
    spans.extend(right);
    Line::from(spans)
}

/// Width of the ctx% value-ramp bar, in cells.
const CTX_BAR_W: usize = 10;

/// Build the ctx% value-ramp: `filled` cells lightened toward white by
/// [`rail::value_step`] of the percentage (so near-full glows), then
/// `CTX_BAR_W - filled` dim slate cells.  Reuses the rail's ramp so the
/// bar and the marginal rail share one value scale.
fn ctx_ramp(pct: u64, bar_w: usize) -> Vec<Span<'static>> {
    let pct = pct.min(100) as usize;
    let filled = ((pct as f64 / 100.0) * bar_w as f64).round() as usize;
    let filled = filled.min(bar_w);
    let step = rail::value_step(Some(pct as u32));
    let fill_col = rail::lighten(CYAN, step);
    let mut spans = Vec::with_capacity(bar_w);
    spans.push(Span::styled("ctx ", Style::default().fg(SLATE)));
    for _ in 0..filled {
        spans.push(Span::styled("█", Style::default().fg(fill_col)));
    }
    for _ in filled..bar_w {
        spans.push(Span::styled("░", Style::default().fg(SLATE)));
    }
    spans.push(Span::styled(" ", Style::default().fg(SLATE)));
    spans
}

/// Width of the elapsed-wait bar, in cells.
const WAIT_BAR_W: usize = 10;

/// Bucket whole seconds of elapsed phase time into a `0..=3` value step
/// for the wait bar's colour: a normal sub-10s phase stays dim, a
/// dragging one flares toward white past ~30s. Deliberately distinct
/// from [`rail::value_step`], which is calibrated for line counts
/// (4/20/80) — feeding that ramp milliseconds is what saturated the old
/// duration ribbon to white on every turn.
fn wait_step(secs: u64) -> u8 {
    match secs {
        0..=9 => 0,
        10..=19 => 1,
        20..=29 => 2,
        _ => 3,
    }
}

/// Build the elapsed-wait bar: [`WAIT_BAR_W`] cells whose filled run
/// grows on a `log2` scale with the current phase's elapsed seconds
/// (empty at 0s, ~7 cells near 16s, full near a minute), then a ` Ns `
/// readout. The fill colour is [`PURPLE`] lightened by [`wait_step`] —
/// dim while the wait is normal, bright when it drags — so size and
/// value agree, reusing the rail's [`rail::lighten`] ramp. PURPLE (not
/// the ctx ramp's CYAN) keeps the two bottom bars visually distinct.
fn wait_bar(elapsed: Duration) -> Vec<Span<'static>> {
    let secs = elapsed.as_secs();
    // log2 fill, scaled so a minute-long wait reaches the right edge:
    // 0s → 0 cells, 3s → ~3, 16s → ~7, ~60s → full.
    let filled = ((((secs + 1) as f64).log2() * 1.7).round() as usize).min(WAIT_BAR_W);
    let fill_col = rail::lighten(PURPLE, wait_step(secs));
    let mut spans = Vec::with_capacity(WAIT_BAR_W + 1);
    for _ in 0..filled {
        spans.push(Span::styled("█", Style::default().fg(fill_col)));
    }
    for _ in filled..WAIT_BAR_W {
        spans.push(Span::styled("░", Style::default().fg(SLATE)));
    }
    spans.push(Span::styled(
        format!(" {secs}s "),
        Style::default().fg(SLATE),
    ));
    spans
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
    tabs: &[AgentId],
    titles: &HashMap<AgentId, String>,
    focused: AgentId,
    dying: &HashMap<AgentId, Instant>,
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

/// Columns the matrix label is truncated/padded to, so the step cells,
/// token readout, and size bar align into a grid down the rows.
const MATRIX_LABEL_W: usize = 10;
/// Most-recent step cells a matrix row shows; a longer run keeps the tail.
const MATRIX_STEPS_W: usize = 8;

/// The multi-agent matrix: one row per live session, columns
/// `label  steps  tokens  sizebar  Nst`.  Rows = agents in `sort` order,
/// coloured by each agent's rail hue so the matrix and the rail share one
/// identity.  A *projection* of the existing `tabs`/`viewports` model —
/// with a single session it collapses to [`tab_bar`]'s exact output, so
/// the common case is visually unchanged.
///
/// `rows` pairs each tab's id with its viewport (matrix figures are
/// derived from the viewport: step cells, lines touched, token spend);
/// `titles`/`focused`/`dying` carry the same row state `tab_bar` reads.
fn matrix_bar(
    rows: &[(AgentId, &Viewport)],
    titles: &HashMap<AgentId, String>,
    focused: AgentId,
    dying: &HashMap<AgentId, Instant>,
    sort: MatrixSort,
) -> Vec<Line<'static>> {
    if rows.len() <= 1 {
        let tabs: Vec<AgentId> = rows.iter().map(|(id, _)| *id).collect();
        return vec![tab_bar(&tabs, titles, focused, dying)];
    }
    // Render-time row order: spawn keeps `rows` (the `tabs` order); cost
    // sorts by cumulative spend, descending, so the budget-burner floats
    // to the top.  A stable sort preserves spawn order among ties.
    let mut order: Vec<usize> = (0..rows.len()).collect();
    if sort == MatrixSort::Cost {
        order.sort_by_key(|&i| {
            let u = rows[i].1.usage();
            std::cmp::Reverse(u.input + u.output)
        });
    }
    // The value ramp is relative to the heaviest spender this frame: the
    // top row(s) read near-white, the rest step down, so "which child
    // burned the budget" is pre-attentive even though raw token counts
    // dwarf `rail::value_step`'s line-count thresholds.
    let max_tokens = rows
        .iter()
        .map(|(_, vp)| {
            let u = vp.usage();
            u.input + u.output
        })
        .max()
        .unwrap_or(0);
    order
        .into_iter()
        .map(|i| {
            let (id, vp) = rows[i];
            matrix_row(id, vp, titles, focused, dying, max_tokens)
        })
        .collect()
}

/// One matrix row: `label  steps  tokens  sizebar  Nst`, hued by the
/// agent's rail slot.  Focused row bold; dying rows dim and carry the
/// `LINGER` countdown in place of the size bar's right margin.
fn matrix_row(
    id: AgentId,
    vp: &Viewport,
    titles: &HashMap<AgentId, String>,
    focused: AgentId,
    dying: &HashMap<AgentId, Instant>,
    max_tokens: u64,
) -> Line<'static> {
    let hue = AGENT_HUES
        .get(vp.agent().0 as usize)
        .copied()
        .unwrap_or(AGENT_HUES[0]);
    let dim = dying.contains_key(&id);

    // Label: truncated/padded to a fixed column, focused in brackets.
    let title = titles.get(&id).map(String::as_str).unwrap_or("?");
    let truncated: String = title.chars().take(MATRIX_LABEL_W).collect();
    let label = if id == focused {
        format!("[{truncated}]")
    } else {
        format!(" {truncated} ")
    };
    let pad = (MATRIX_LABEL_W + 2).saturating_sub(label.chars().count());
    let mut label_style = Style::default().fg(hue);
    if id == focused {
        label_style = label_style.add_modifier(Modifier::BOLD);
    }
    if dim {
        label_style = label_style.add_modifier(Modifier::DIM);
    }
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(label, label_style),
        Span::raw(" ".repeat(pad + 1)),
    ];

    // Step cells: `●` step-with-tool-call, `○` without; the most-recent
    // window so a long run never overruns the row.  A done/dying session
    // leads with `√`, an errored one with `╳`.
    spans.push(Span::styled(step_cells(vp, dim), Style::default().fg(hue)));

    // Token readout: cumulative spend, lightened toward white in
    // proportion to the heaviest spender this frame.
    let tokens = {
        let u = vp.usage();
        u.input + u.output
    };
    let value = relative_value_step(tokens, max_tokens);
    let token_style = Style::default()
        .fg(rail::lighten(hue, value))
        .add_modifier(if dim {
            Modifier::DIM
        } else {
            Modifier::empty()
        });
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!("{:>6}", provider::humanize_tokens(tokens)),
        token_style,
    ));

    // Size readout: a `▓`-bar over lines touched (Phase 3's size-bar
    // idiom), then the step count as `Nst`.
    let touched = vp.lines_touched();
    spans.push(Span::raw("  "));
    spans.push(line::size_bar(touched));
    spans.push(Span::styled(
        format!("  {}st", vp.steps().len()),
        Style::default().fg(SLATE).add_modifier(if dim {
            Modifier::DIM
        } else {
            Modifier::empty()
        }),
    ));

    if dim && let Some(t) = dying.get(&id) {
        let left = LINGER.saturating_sub(t.elapsed()).as_secs();
        spans.push(Span::styled(
            format!(" ({left}s)"),
            Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        ));
    }
    Line::from(spans)
}

/// The matrix row's step glyphs: `done` leads the cell run with `√`
/// (session in its linger window) or `╳` (last block an error); otherwise
/// each step renders `●` (a tool call landed within it) or `○` (none).
/// Capped to [`MATRIX_STEPS_W`] keeping the most-recent steps.
fn step_cells(vp: &Viewport, dying: bool) -> String {
    let steps = vp.steps();
    let tail = steps.len().saturating_sub(MATRIX_STEPS_W);
    let mut s = String::new();
    if dying {
        s.push(if vp.last_is_error() { '╳' } else { '√' });
    }
    let room = MATRIX_STEPS_W.saturating_sub(s.chars().count());
    for &had_call in steps[tail..].iter().rev().take(room).rev() {
        s.push(if had_call { '●' } else { '○' });
    }
    s
}

/// Bucket `tokens` against the frame's `max_tokens` into a `0..=3`
/// value step for [`rail::lighten`]: the heaviest spender reads brightest,
/// the rest step down by quartile of the maximum.  `max_tokens == 0`
/// (no spend yet) reads flat at the base hue.
fn relative_value_step(tokens: u64, max_tokens: u64) -> u8 {
    if max_tokens == 0 {
        return 0;
    }
    (tokens * 3).div_ceil(max_tokens).min(3) as u8
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

/// Resolve a user-typed `/export` path: expand a leading `~`/`xdg:` sigil
/// against the home dir, then anchor a still-relative path at `cwd` (where
/// exarch was launched) so `/export notes.md` lands there rather than in
/// whatever directory the process happens to sit in.  [`resolve_str`] folds
/// `.`/`..` and joins the cwd.
fn resolve_export_path(arg: &str, cwd: &str) -> PathBuf {
    let expanded = expand_path_prefix(arg, &ral_core::host::home());
    ral_core::path::resolve_str(Some(cwd), &expanded)
}

fn footer_hint() -> Line<'static> {
    let st = Style::default()
        .fg(SLATE)
        .add_modifier(Modifier::DIM | Modifier::ITALIC);
    let hint = " Tab pane • ⇧Tab reorder • click ▸ expand • wheel ▸ dial • drag copy (⇧ native) • wheel/PgUp scroll • Ctrl-X Ctrl-E editor • Ctrl-C cancel • /quit to leave ";
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

/// Pairs the terminal lifetime with the app so the worker thread and the UI
/// loop can split the two: the worker borrows the session through
/// [`App::handle`]'s bus, the UI loop borrows `guard.term()` alongside
/// `&mut self.app` via direct field syntax for disjoint-borrow splitting.
pub struct Tui {
    guard: TerminalGuard,
    app: App,
}

impl Tui {
    pub fn new(
        root_id: AgentId,
        root_log_dir: &Path,
        context_window: Option<u64>,
        stderr_log: &Path,
        vi: bool,
    ) -> io::Result<Self> {
        let guard = TerminalGuard::enter(stderr_log)?;
        let app = App::new(root_id, root_log_dir, context_window, vi);
        Ok(Self { guard, app })
    }
}

/// One row of the slash-command registry: the canonical token, any aliases,
/// the argument it consumes (if any), and a one-line description for `/help`.
/// The table is metadata only — names, help, and the argument shape; dispatch
/// is a direct match by name in [`route_submit`], split by where the work must
/// run (the UI thread or the session's drive loop).
struct SlashCommand {
    name: &'static str,
    aliases: &'static [&'static str],
    /// The trailing argument the command consumes, e.g. `Some("<path>")`
    /// for `/export`.  `None` marks an argument-less command, which
    /// [`lookup_command`] matches only when typed alone — trailing text
    /// means the user meant a prompt, not the command.  Shown in `/help`.
    arg: Option<&'static str>,
    help: &'static str,
}

/// The slash-command registry — the single source of truth for the prompt-box
/// highlight ([`is_slash_command`]), the routing match ([`route_submit`]), and
/// the `/help` listing, so the three cannot drift.
const SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/help",
        aliases: &[],
        arg: None,
        help: "List the available commands.",
    },
    SlashCommand {
        name: "/legend",
        aliases: &[],
        arg: None,
        help: "Decode the rail, bars, grain, and fidelity treatments.",
    },
    SlashCommand {
        name: "/clear",
        aliases: &[],
        arg: None,
        help: "Forget the conversation and clear the screen.",
    },
    SlashCommand {
        name: "/copy",
        aliases: &[],
        arg: None,
        help: "Copy the latest reply to the clipboard.",
    },
    SlashCommand {
        name: "/export",
        aliases: &[],
        arg: Some("<path>"),
        help: "Write the user view to a file.",
    },
    SlashCommand {
        name: "/model",
        aliases: &[],
        arg: None,
        help: "Switch the model or provider.",
    },
    SlashCommand {
        name: "/compact",
        aliases: &[],
        arg: None,
        help: "Summarize the conversation to reclaim context.",
    },
    SlashCommand {
        name: "/quit",
        aliases: &["/exit"],
        arg: None,
        help: "Leave exarch.",
    },
];

/// The command named by `trimmed`'s first token together with the trailing
/// argument it consumes, if any.  The first whitespace-delimited token is
/// matched against each command's name and aliases; the remainder, trimmed,
/// is the argument.  An argument-less command ([`SlashCommand::arg`] `None`)
/// matches only when typed alone — trailing text means the user meant a
/// prompt, so it declines and the line proceeds to the model.
fn lookup_command(trimmed: &str) -> Option<(&'static SlashCommand, &str)> {
    let (head, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (trimmed, ""),
    };
    let cmd = SLASH_COMMANDS
        .iter()
        .find(|c| c.name == head || c.aliases.contains(&head))?;
    if cmd.arg.is_none() && !rest.is_empty() {
        return None;
    }
    Some((cmd, rest))
}

/// Whether `text`, as typed, is a recognized slash command — its first
/// token matched, mirroring [`lookup_command`]'s dispatch (so an
/// argument-less command with trailing text reads as a prompt, not a
/// command).
fn is_slash_command(text: &str) -> bool {
    lookup_command(text.trim()).is_some()
}

/// The agent-affecting slash command hook the worker's [`Agent::drive`]
/// calls at the turn boundary, where the drive thread owns the agent the
/// command mutates.  `/clear` rebuilds the agent's context (its viewport was
/// already cleared UI-side), `/compact` summarizes the history, `/quit` ends
/// the drive loop — which sets `done`, so the UI loop's next drain returns
/// `Stop` and exits.  Every other command is handled UI-side and never reaches
/// here.  Only the trunk drives with this `Control` (a sub-agent uses
/// [`NoControl`](crate::agent::NoControl)), so a slash command always targets
/// the trunk's own context and provider.
struct ReplControl<'a> {
    scratch: &'a Scratch,
}

impl Control for ReplControl<'_> {
    fn command(&mut self, raw: &str, session: &mut Agent, emit: &Emitter) -> ControlFlow {
        match raw.trim() {
            "/clear" => {
                let _ = session.clear(self.scratch);
                ControlFlow::Continue
            }
            "/compact" => {
                let p = session.current_provider();
                let token = session.cancel_token().clone();
                session.compact(&p, emit, true, &token);
                ControlFlow::Continue
            }
            "/quit" | "/exit" => ControlFlow::Quit,
            _ => ControlFlow::Continue,
        }
    }
}

/// Build the [`Tui`], banner, run the worker + UI loop, flush logs, print log
/// paths + usage on the restored shell.
#[allow(clippy::too_many_arguments)]
pub fn run(
    session: &mut Agent,
    provider: Arc<Provider>,
    info: &SessionInfo<'_>,
    store: &CredentialStore,
    catalog: &mut ModelCatalog<LiveSource>,
    scratch: &Scratch,
    run_dir: &std::path::Path,
    seed: Option<String>,
    vi: bool,
) -> Result<(), String> {
    let caps = provider::caps_for(provider.model());
    let stderr_log = run_dir.join("stderr.log");
    let mut tui = Tui::new(
        session.id,
        session.log_dir(),
        caps.context_window,
        &stderr_log,
        vi,
    )
    .map_err(|e| format!("ratatui init: {e}"))?;
    let status_provider = crate::oauth::provider_label(provider.subscription(), info.provider);
    tui.app.set_status_model(&status_provider, info.model);
    // Bind the App's inbox and focus to the trunk's shared handles, then build
    // the fleet: a session-lived bus over the trunk's inbox, plus the shared
    // registry and focus handle.  Input, the pending strip, async-agent
    // results, and the worker's drive loop all read and write this one inbox;
    // `TAB` and the focused agent's park predicate share one focus handle.
    tui.app.bind_inbox(session.inbox());
    tui.app.bind_focus(session.focus_handle());
    let fleet = Fleet::new(
        session.agents.clone(),
        FleetBus::session(session.inbox()),
        session.focus_handle(),
        session.interactive(),
    );
    if let Some(s) = seed {
        session.seed(s);
    }
    tui.app
        .banner(tui.guard.term(), info)
        .map_err(|e| e.to_string())?;

    // The worker thread runs the trunk via `Agent::drive`, parking on an empty
    // inbox (the conversing trunk) until a `/quit` command tells its `Control`
    // to quit; it then sets `done`, and the UI loop's next drain returns
    // `Stop`. The UI loop renders the bus and routes input in one continuous
    // loop alongside it.  The trunk drives on its own provider handle.
    let done = AtomicBool::new(false);
    let done_ref = &done;
    let mut control = ReplControl { scratch };
    // The worker captures the trunk's emitter, not `&fleet.bus`: `FleetBus` is
    // not `Sync` (its `Receiver` is single-consumer), so the receiver stays on
    // the UI thread. The emitter is `Send` and is all the worker needs.  It
    // carries the trunk's `Transcript`, so the TUI records `transcript.jsonl`
    // too — the operational view beside `user.log`'s rendered one.
    let worker_emit = fleet.bus.emitter(session.id, session.transcript());
    // A recording emitter for the UI thread, minted from the bus *before* the
    // worker takes the trunk: it carries the trunk's `transcript()`, so a
    // UI-caused operational event — a `/model` switch — records in the trace
    // and draws through the normal bus path, exactly as a worker-raised note
    // does.  The worker takes `worker_emit`; this clone stays on the UI thread.
    let ui_emit = fleet.bus.emitter(session.id, session.transcript());
    // A `Mailbox` onto the trunk inbox, so a UI-loop failure can wake the
    // parked worker with a `/quit` before joining — without it the conversing
    // trunk parks forever and `join` would deadlock.
    let quit_mailbox = session.inbox().mailbox();
    // The UI thread's command context: the handles `route_submit` and the
    // `/model` path service a submitted line against, threaded as one.  The
    // registry is the same shared map the worker mutates, so an agent it
    // registers is visible to the UI at once — for steering, `wake`, and a
    // `/model` swap on the focused agent's handle.
    let mut cmd_ctx = CommandCtx {
        agents: &fleet.agents,
        store,
        catalog,
        info,
        emit: &ui_emit,
    };
    std::thread::scope(|scope| -> Result<(), String> {
        let worker = scope.spawn(move || {
            let out = session.drive(&mut control, &worker_emit);
            done_ref.store(true, Ordering::Release);
            out
        });
        let r = ui_loop(&mut tui, &fleet.bus, done_ref, &mut cmd_ctx);
        if r.is_err() {
            quit_mailbox.push(InboxMsg::Command("/quit".into()));
        }
        let _ = worker.join();
        r.map_err(|e| e.to_string())
    })?;

    let logs = tui
        .app
        .flush_logs()
        .map_err(|e| format!("session logs: {e}"));
    let usage = tui.app.total_usage();
    // Restore the terminal before printing so log paths land on the
    // user's normal shell rather than the alt screen.
    drop(tui);
    if let Ok(paths) = &logs {
        for p in paths {
            match p.parent() {
                Some(dir) => println!("Agent logs: {} (user.log + events.json)", dir.display()),
                None => println!("Agent log: {}", p.display()),
            }
        }
    } else if let Err(e) = logs {
        eprintln!("exarch: {e}");
    }
    println!("{usage}");
    Ok(())
}

/// Channel carrying `(provider, fetched models or failure)` from the
/// per-provider background fetch threads back to the picker loop.
type FetchRx = std::sync::mpsc::Receiver<(provider::ProviderId, Result<Vec<String>, String>)>;

/// The long-lived handles the UI thread services a submitted line against: the
/// fleet registry (for steering, `wake`, and the focused agent's provider
/// handle a `/model` swap targets), the credential store and model catalog the
/// `/model` picker reads, the static session info, and the recording emitter a
/// UI-caused operational event (a model switch) rides.  Bundled so the command
/// path — `ui_loop` → `route_submit` → `pick_model` → `apply_model_switch` —
/// threads one context rather than a fistful of handles.
struct CommandCtx<'a> {
    agents: &'a AgentRegistry,
    store: &'a CredentialStore,
    catalog: &'a mut ModelCatalog<LiveSource>,
    info: &'a SessionInfo<'a>,
    emit: &'a Emitter,
}

/// The merged render + input loop, running on the UI thread alongside the
/// worker's [`Agent::drive`].  It drains the session-lived bus into the App
/// (the same `App::handle` the old per-turn drive used), ticks and redraws at
/// ~60 FPS, and routes the user's keystrokes: scrollback / picker keys edit the
/// App, a submitted line is routed by [`route_submit`] (view commands run here;
/// agent commands and plain prompts go onto the focused agent's inbox), and Esc
/// / Ctrl-C cancel the focused agent's turn and its subtree.  A `TAB` that moves
/// focus `wake`s the de-focused and newly-focused agents so each re-evaluates
/// its park verdict.  Returns when the worker finishes (a `/quit`), draining its
/// final events for one last frame.
fn ui_loop(
    tui: &mut Tui,
    bus: &FleetBus,
    done: &AtomicBool,
    ctx: &mut CommandCtx<'_>,
) -> io::Result<()> {
    const BATCH: usize = 64;
    let frame = Duration::from_millis(16); // ~60 FPS max
    // The session inbox, so a routed line (a plain prompt, a session command)
    // reaches the worker's drive loop through the queue the App is bound to.
    let mailbox = tui.app.inbox.mailbox();
    let rx = bus.rx();
    // The frame clock: the instant the last frame was painted, seeded a frame
    // in the past so the first iteration paints at once.  Draws are gated on it
    // so the redraw rate is bounded by the frame interval independently of how
    // fast events drain — a token/tool flood coalesces into one coherent frame
    // per interval instead of a full-screen rewrite per 64-event batch (the
    // jitter that churn caused).
    let mut last_draw = Instant::now() - frame;
    loop {
        // Focus as of the start of this iteration; compared at the end so a
        // `TAB`, or a focused agent ending mid-drain, wakes the agents whose
        // park verdict just changed.
        let prev_focus = tui.app.focused();
        // The explicit-done completion contract (shared with the headless
        // `Sink::drive`): drain a batch, then stop only when the worker is
        // *done* — never when the channel empties or disconnects, so a detached
        // worker (a live background `agent`) flooding the bus cannot end the
        // loop early. The batch cap bounds how long a token flood can starve the
        // input poll below; `More` means events are still queued, so the frame
        // does not wait for one.
        let more = match drain_pass(rx, done, Some(BATCH), |ev| tui.app.handle(ev)) {
            Pass::Stop => {
                tui.app.busy_off();
                tui.app.draw(tui.guard.term())?;
                return Ok(());
            }
            Pass::More => true,
            Pass::Idle => false,
        };
        // Paint only when a frame is due, so a multi-batch backlog still drains
        // at full throughput but redraws at most once per interval.  Idle frames
        // are still due each interval, so the animated wait bar keeps ticking.
        if last_draw.elapsed() >= frame {
            tui.app.tick();
            tui.app.draw(tui.guard.term())?;
            last_draw = Instant::now();
        }
        // Poll for input every iteration, even with events still queued: a
        // backlog of streamed tokens must never starve Esc/Ctrl-C. While the
        // drain is incomplete the poll is non-blocking so draining stays prompt;
        // once the channel is empty it waits only until the next frame is due,
        // which both paces the idle loop and keeps Esc/Ctrl-C responsive.
        let timeout = if more {
            Duration::ZERO
        } else {
            frame.saturating_sub(last_draw.elapsed())
        };
        if ct_poll(timeout)? {
            match ct_read()? {
                CtEvent::Key(k) if k.kind == KeyEventKind::Press => {
                    // A tab is steerable when it is root (slash commands and
                    // prompts) or a live peer with a registered inbox; on a
                    // steerable tab Enter submits and text entry is allowed.
                    let focused = tui.app.focused();
                    let steerable =
                        focused == tui.app.root || ctx.agents.mailbox(focused).is_some();
                    match key_action(KeyMode::Running, &k, steerable) {
                        // Esc / Ctrl-C cancel the *focused* agent's turn and its
                        // subtree.  On the trunk that is the published-slot path
                        // (the token and the ral foreground); the cascade then
                        // reaps any descendants.  On a focused sub-agent only the
                        // registry cascade fires — its eval is detached, not the
                        // foreground, so the slot/foreground path would target
                        // the trunk by mistake.
                        KeyAction::Cancel => {
                            if focused == tui.app.root {
                                cancel::raise_interrupt();
                            }
                            ctx.agents.cancel(focused);
                        }
                        KeyAction::Submit => {
                            if let Some(text) = tui.app.submit() {
                                if focused == tui.app.root {
                                    route_submit(text, tui, &mailbox, ctx)?;
                                } else if let Some(mb) = ctx.agents.mailbox(focused) {
                                    // Steer the focused agent: the whole line is
                                    // its next turn — no slash, no revival.
                                    mb.push_user(text);
                                }
                                // The agent died between focus and submit: its
                                // mailbox is gone, so the line is dropped.
                            }
                        }
                        KeyAction::Edit => {
                            tui.app.key(k, steerable);
                            if tui.app.take_editor_request() {
                                compose_in_editor(tui)?;
                            }
                        }
                    }
                }
                CtEvent::Paste(s) => tui.app.paste(&s),
                CtEvent::Mouse(m) => tui.app.mouse(m),
                _ => {}
            }
        }
        // A focus change this iteration (a `TAB`, or a focused agent ending)
        // wakes both the de-focused and newly-focused agents, so each
        // re-evaluates its park verdict: the de-focused, unscheduled, idle one
        // flips to `Quiesce` and reaps; the newly-focused one stays `Held`.
        let now_focus = tui.app.focused();
        if now_focus != prev_focus {
            if let Some(mb) = ctx.agents.mailbox(prev_focus) {
                mb.wake();
            }
            if let Some(mb) = ctx.agents.mailbox(now_focus) {
                mb.wake();
            }
        }
    }
}

/// Route a submitted prompt line.  A view command (`/help`, `/legend`, `/copy`,
/// `/export`, `/model`) touches only the App, clipboard, file, or picker, so it
/// runs here on the UI thread.  A session command (`/clear`, `/compact`,
/// `/quit`) and a plain prompt go onto the session inbox, where the worker's
/// drive loop drains them — `/clear` *also* clears the viewport UI-side so the
/// screen blanks immediately, before the worker rebuilds the session.
fn route_submit(
    text: String,
    tui: &mut Tui,
    mailbox: &Mailbox,
    ctx: &mut CommandCtx<'_>,
) -> io::Result<()> {
    let info = ctx.info;
    let trimmed = text.trim();
    match lookup_command(trimmed) {
        Some((cmd, arg)) => match cmd.name {
            "/help" => cmd_help(&mut tui.app),
            "/legend" => cmd_legend(&mut tui.app),
            "/copy" => cmd_copy(&mut tui.app),
            "/export" => cmd_export(&mut tui.app, arg, info),
            "/model" => {
                pick_model(tui, ctx)?;
            }
            // The viewport blanks immediately, and the in-flight model response
            // is cancelled first — otherwise streamed tokens sitting in the bus
            // keep flowing into the cleared viewport until the worker, parked
            // inside `apply`, hits its next poll (50 ms) and the model's turn
            // ends on its own.  Raising the interrupt cancels the trunk's
            // published token and the ral foreground, exactly as Esc does; the
            // subtree cascade reaps any live descendants now rather than after
            // the worker reaches the `Turn::Command`.  Stragglers already in the
            // unbounded bus channel are dropped in `App::handle` by the
            // clear-drain guard `root_clear_drain` arms.  Then the `/clear`
            // itself reaches the worker's drive loop and rebuilds the session.
            "/clear" => {
                let root = tui.app.root;
                cancel::raise_interrupt();
                ctx.agents.cancel(root);
                tui.app.clear(info, tui.guard.term())?;
                mailbox.push(InboxMsg::Command("/clear".into()));
            }
            // The worker's `ReplControl` compacts the history / returns Quit.
            _ => mailbox.push(InboxMsg::Command(text.clone())),
        },
        // A plain prompt: onto the session inbox for the worker to drain.
        None => mailbox.push_user(text),
    }
    Ok(())
}

/// Emit one dim transcript line per registry entry: the command token
/// (with aliases) left-padded to a common width, then its description.
fn cmd_help(app: &mut App) {
    let id = app.root;
    let names: Vec<String> = SLASH_COMMANDS
        .iter()
        .map(|c| {
            let mut s = c.name.to_string();
            if let Some(arg) = c.arg {
                s.push(' ');
                s.push_str(arg);
            }
            if !c.aliases.is_empty() {
                s.push_str(&format!(" ({})", c.aliases.join(", ")));
            }
            s
        })
        .collect();
    let width = names.iter().map(String::len).max().unwrap_or(0);
    for (n, c) in names.iter().zip(SLASH_COMMANDS) {
        app.push_note(id, format!("{n:<width$}   {}", c.help));
    }
}

/// Push the visual-vocabulary legend onto the transcript as ambient, rail-less
/// chrome — the panel that decodes the rail, bars, grain, and fidelity
/// treatments, rendered as the graphic's own samples.
fn cmd_legend(app: &mut App) {
    app.push_chrome(app.root, RailShape::Plain, legend_panel());
}

/// Copy the latest assistant reply — the focused tab's trailing prose, as raw
/// markdown — to the system clipboard via OSC 52.  An oversized reply exceeds
/// the terminal's per-sequence limit, so copy its tail (bounded by `YANK_CAP`)
/// and say so, rather than let the terminal drop the whole sequence and copy
/// nothing silently.
fn cmd_copy(app: &mut App) {
    let id = app.root;
    let reply = app.latest_reply();
    if reply.is_empty() {
        app.push_error(id, "no reply to copy yet".into());
        return;
    }
    let payload = tail_bytes(&reply, YANK_CAP);
    if let Err(e) = osc52_copy(payload) {
        app.push_error(id, format!("clipboard write failed: {e}"));
        return;
    }
    let note = if payload.len() < reply.len() {
        format!("[reply exceeds the clipboard limit — copied its last {YANK_CAP} bytes]")
    } else {
        format!(
            "[copied the latest reply — {} lines]",
            reply.lines().count()
        )
    };
    app.push_note(id, note);
}

/// Write the focused tab's user view — its rendered `user.log` — to `arg`, a
/// path that may be absolute, relative to the launch cwd, or `~`/`xdg:`-
/// prefixed.  Refuses to overwrite an existing file so an export never clobbers;
/// an empty argument prints the usage line.  The copy itself goes through
/// [`viewport::export_log`], where the `user.log` I/O door lives.
fn cmd_export(app: &mut App, arg: &str, info: &SessionInfo<'_>) {
    let id = app.root;
    if arg.is_empty() {
        app.push_error(id, "usage: /export <path>".into());
        return;
    }
    let dest = resolve_export_path(arg, info.cwd);
    if dest.exists() {
        app.push_error(id, format!("refusing to overwrite {}", dest.display()));
        return;
    }
    let src = match app.flush_focused_log() {
        Ok(p) => p,
        Err(e) => {
            app.push_error(id, format!("could not flush transcript: {e}"));
            return;
        }
    };
    match viewport::export_log(&src, &dest) {
        Ok(_) => app.push_note(id, format!("[exported user view to {}]", dest.display())),
        Err(e) => app.push_error(id, format!("could not write {}: {e}", dest.display())),
    }
}

/// Open the `/model` picker over the available providers, fetch their model
/// lists (cache-first, then background), and drive the modal loop until the
/// user selects a model or dismisses it. On a selection the provider is rebuilt
/// over the same transcript, the [`ProviderHandle`] is swapped (taking effect on
/// the worker's next turn), the saved selection is updated, and the status bar
/// follows.
fn pick_model(tui: &mut Tui, ctx: &mut CommandCtx<'_>) -> io::Result<()> {
    let store = ctx.store;
    let available = store.available();
    // Each plan-backed provider's flavour, for the picker's labels: a ChatGPT
    // login (the OAuth credential) reads as the ChatGPT plan, an otherwise-
    // metered provider whose `ProviderId` declares a flat rate (opencode Go) as
    // the generic subscription. A provider absent from the map is metered.
    let subscription = available
        .iter()
        .filter_map(|id| {
            let kind = if store.get(id).is_some_and(|c| c.is_subscription()) {
                crate::oauth::Subscription::ChatGpt
            } else if id.flat_rate() {
                crate::oauth::Subscription::FlatRate
            } else {
                return None;
            };
            Some((id.clone(), kind))
        })
        .collect();
    // Seed the tuning controls from the focused provider's live values, so the
    // overlay opens showing the effort/temperature currently in force (a
    // settled agent with no live handle falls back to the defaults).
    let initial_tuning = ctx
        .agents
        .provider(tui.app.focused())
        .map(|p| p.current().tuning().clone())
        .unwrap_or_default();
    let mut picker = Picker::new(available, subscription, initial_tuning, provider::caps_for);
    // Seed each provider from the catalog's cache instantly; spawn a background
    // fetch for the rest so the UI shows "loading…" rather than freezing on the
    // network. A ChatGPT plan login has no catalog endpoint, so its curated plan
    // models are seeded directly and it is excluded from the fetch; a flat-rate
    // gateway (opencode Go) lists live through genai like any other API-key
    // provider.
    let mut rx = None;
    let to_fetch: Vec<_> = picker
        .loading_providers()
        .into_iter()
        .filter(|id| {
            if store.get(id).is_some_and(|c| c.is_subscription()) {
                let models = crate::oauth::PLAN_MODELS
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                picker.set_models(id, picker::ModelsState::Loaded(models));
                return false;
            }
            match ctx.catalog.cached(id) {
                Some(models) => {
                    picker.set_models(id, picker::ModelsState::Loaded(models));
                    false
                }
                None => true,
            }
        })
        .collect();
    if !to_fetch.is_empty() {
        let (tx, recv) = std::sync::mpsc::channel();
        for id in to_fetch {
            let source = ctx.catalog.source().clone();
            let tx = tx.clone();
            std::thread::spawn(move || {
                let result = source.list(&id);
                let _ = tx.send((id, result));
            });
        }
        rx = Some(recv);
    }
    tui.app.picker = Some(picker);
    let outcome = drive_picker(tui, store, ctx.catalog, rx);
    tui.app.picker = None;
    if let Some((id, model, tuning)) = outcome {
        apply_model_switch(tui, ctx, id, model, tuning);
    }
    Ok(())
}

/// Poll keys and background-fetch results until the picker resolves.  Returns
/// the chosen `(provider, model, tuning)`, or `None` on cancel.
fn drive_picker(
    tui: &mut Tui,
    store: &CredentialStore,
    catalog: &mut ModelCatalog<LiveSource>,
    rx: Option<FetchRx>,
) -> Option<(provider::ProviderId, String, provider::Tuning)> {
    loop {
        // Fold any landed fetch results into the picker (and the catalog's
        // caches), on this thread, so the disk write stays single-threaded.
        if let Some(rx) = &rx {
            while let Ok((id, result)) = rx.try_recv() {
                let state = match result {
                    Ok(models) => {
                        catalog.record(&id, models.clone());
                        picker::ModelsState::Loaded(models)
                    }
                    Err(reason) => picker::ModelsState::Failed(reason),
                };
                if let Some(p) = tui.app.picker_mut() {
                    p.set_models(&id, state);
                }
            }
        }
        if tui.app.draw(tui.guard.term()).is_err() {
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
        if key_action(KeyMode::Overlay, &k, false) == KeyAction::Cancel {
            return None;
        }
        let action = tui.app.picker_mut()?.key(k.code);
        match action {
            picker::PickAction::None => {}
            picker::PickAction::Cancelled => return None,
            picker::PickAction::Selected(id, model, tuning) => {
                return Some((id, model, tuning));
            }
            picker::PickAction::Manual(query, tuning) => {
                let available = store.available();
                match crate::models::resolve_model_provider(&query, &available, catalog) {
                    Ok(id) => return Some((id, query, tuning)),
                    Err(e) => {
                        let root = tui.app.root;
                        tui.app.push_error(root, e);
                    }
                }
            }
        }
    }
}

/// Rebuild the provider for the chosen `kind` + `model` over the same
/// transcript and swap it into the **focused agent's** own provider handle
/// (its next turn reads it), persist the selection to the project state dir,
/// and update the live status bar. A persistence failure is noted but does not
/// undo the in-memory switch.  A focused agent that settled between the picker
/// opening and the selection has no handle to swap, so the switch is dropped.
///
/// A model switch is a *real* operational event, so it goes through `emit` —
/// the UI-thread recording emitter carrying the trunk's transcript — as a
/// [`Kind::SystemNote`].  It records in the trace and draws through the normal
/// bus path, like a worker-raised note; the UI never fabricates an `Event` for
/// it.  Its own failures, by contrast, are view chrome ([`App::push_error`]).
fn apply_model_switch(
    tui: &mut Tui,
    ctx: &CommandCtx<'_>,
    provider_id: provider::ProviderId,
    model: String,
    tuning: provider::Tuning,
) {
    let store = ctx.store;
    let info = ctx.info;
    let emit = ctx.emit;
    let id = tui.app.root;
    let Some(cred) = store.get(&provider_id).cloned() else {
        tui.app.push_error(
            id,
            format!("{} has no resolved credential", provider_id.label()),
        );
        return;
    };
    // Swap the *focused* agent's handle; if it has settled, there is nothing to
    // swap and the selection is dropped (the user can reopen on a live tab).
    let focused = tui.app.focused();
    let Some(provider) = ctx.agents.provider(focused) else {
        tui.app
            .push_error(id, "the focused agent is no longer live".to_string());
        return;
    };
    let new_provider = Arc::new(Provider::build(
        &provider_id,
        model.clone(),
        &cred,
        info.max_tokens_override,
        tuning.clone(),
    ));
    let label = provider_id.label();
    let status_provider = crate::oauth::provider_label(new_provider.subscription(), label);
    provider.swap(new_provider);
    tui.app.set_status_model(&status_provider, &model);
    let state_dir = crate::bootstrap::project_dir(info.cwd);
    if let Err(e) = state::save(&state_dir, &state::State::new(&provider_id, &model, &tuning)) {
        tui.app
            .push_error(id, format!("could not persist selection: {e}"));
    }
    emit.emit(Kind::SystemNote(format!(
        "[switched to {label} {model}{}]",
        tuning_suffix(&tuning)
    )));
}

/// A human-readable suffix for the switch note describing any non-default
/// tuning, e.g. ` · effort high · temp 0.7`. Empty when both knobs are auto.
fn tuning_suffix(tuning: &provider::Tuning) -> String {
    let mut parts = String::new();
    if let Some(effort) = &tuning.effort {
        parts.push_str(&format!(" · effort {}", effort.variant_name()));
    }
    if let Some(temperature) = tuning.temperature {
        parts.push_str(&format!(" · temp {temperature:.1}"));
    }
    parts
}

fn ctrl_key(k: &KeyEvent, c: char) -> bool {
    k.code == KeyCode::Char(c) && k.modifiers.contains(KeyModifiers::CONTROL)
}

/// The two live input contexts: the running UI loop (the worker drives the
/// whole session, so the prompt is never an idle read) and the modal `/model`
/// picker overlay.  There is no idle mode — an interactive root's worker parks
/// in [`Agent::drive`] rather than returning, so the session ends through
/// `/quit`, never a keystroke.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyMode {
    Running,
    Overlay,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KeyAction {
    Edit,
    Submit,
    Cancel,
}

fn key_action(mode: KeyMode, k: &KeyEvent, enter_submits: bool) -> KeyAction {
    if ctrl_key(k, 'c') {
        return KeyAction::Cancel;
    }
    if ctrl_key(k, 'd') {
        return match mode {
            KeyMode::Overlay => KeyAction::Cancel,
            KeyMode::Running => KeyAction::Edit,
        };
    }
    if k.code == KeyCode::Esc {
        return KeyAction::Cancel;
    }
    if enter_submits
        && k.code == KeyCode::Enter
        && !k.modifiers.contains(KeyModifiers::SHIFT)
        && !k.modifiers.contains(KeyModifiers::ALT)
    {
        KeyAction::Submit
    } else {
        KeyAction::Edit
    }
}

#[cfg(test)]
mod banner_tests {
    use super::{SessionInfo, legend_panel, line, rail, session_card};
    use crate::card::{FieldVal, Mark, Role};
    use std::path::{Path, PathBuf};

    /// A representative session: a fetched-catalog model (distinct slug,
    /// known context window), default system prompt, no extend/restrict.
    #[allow(clippy::disallowed_methods)] // test scaffolding: a fixed literal scratch path, no path semantics to get wrong
    fn sample(base: &'static str) -> SessionInfo<'static> {
        SessionInfo {
            provider: "anthropic",
            model: "claude-opus-4-8",
            canonical_slug: Some("claude-opus-4-8-20260101"),
            max_tokens_override: None,
            context_window: Some(200_000),
            max_output_tokens: Some(64_000),
            system_size: 4096,
            system_files: &[],
            base,
            extend_base: None,
            restrict_files: &[],
            scratch: Path::new("/tmp/scratch"),
            cwd: "/Users/me/projects/ral",
        }
    }

    /// The `(label, value)` rows of the single `fields` mark the card carries.
    fn rows(s: &SessionInfo<'_>) -> Vec<(String, FieldVal)> {
        let card = session_card(s);
        match card.marks() {
            [Mark::Fields { rows }] => rows
                .iter()
                .map(|f| (f.label.clone(), f.value.clone()))
                .collect(),
            other => panic!("session card must be one fields mark, got {other:?}"),
        }
    }

    /// The nominal role of a row's leading value span — `None` for a plain
    /// (roleless) quantity readout or a measure.
    fn lead_role(v: &FieldVal) -> Option<Role> {
        match v {
            FieldVal::Inline(spans) => spans.first().and_then(|sp| sp.role),
            FieldVal::Measure(_) => None,
        }
    }

    /// The matrix orders location → identity → capacity → security → prompt,
    /// roles paths as Path, and leaves quantities as plain ink (no hue).
    #[test]
    fn session_card_orders_and_roles_fields() {
        let rs = rows(&sample("read-only"));
        let labels: Vec<&str> = rs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            [
                "cwd",
                "provider",
                "model",
                "context",
                "max-tokens",
                "base",
                "extend-base",
                "restrict",
                "system prompt",
                "scratch",
            ]
        );
        let role = |label: &str| lead_role(&rs.iter().find(|(l, _)| l == label).unwrap().1);
        assert_eq!(role("cwd"), Some(Role::Path), "cwd is a path");
        assert_eq!(role("scratch"), Some(Role::Path), "scratch is a path");
        assert_eq!(role("provider"), Some(Role::Strong), "provider is a name");
        assert_eq!(role("model"), Some(Role::Strong), "model is a name");
        assert_eq!(role("context"), None, "a quantity carries no hue");
        assert_eq!(role("max-tokens"), None, "a quantity carries no hue");
    }

    /// Hue is spent on `base` only when it alarms: `dangerous` → Bad (red),
    /// every safe base → Strong (plain bold).
    #[test]
    fn dangerous_base_is_the_one_field_that_earns_a_hue() {
        let base_role = |b: &'static str| {
            let rs = rows(&sample(b));
            lead_role(&rs.iter().find(|(l, _)| l == "base").unwrap().1)
        };
        assert_eq!(base_role("dangerous"), Some(Role::Bad));
        assert_eq!(base_role("read-only"), Some(Role::Strong));
        assert_eq!(base_role("confined"), Some(Role::Strong));
    }

    /// A distinct canonical slug rides the model row as a muted suffix; an
    /// absent or identical slug leaves the row a single span.
    #[test]
    fn model_slug_is_a_muted_suffix_only_when_distinct() {
        let rs = rows(&sample("read-only"));
        let FieldVal::Inline(spans) = &rs.iter().find(|(l, _)| l == "model").unwrap().1 else {
            panic!("model is an inline value");
        };
        assert_eq!(spans.len(), 2, "distinct slug appends a suffix span");
        assert_eq!(spans[1].role, Some(Role::Muted));
        assert!(spans[1].text.contains("claude-opus-4-8-20260101"));

        let mut same = sample("read-only");
        same.canonical_slug = Some("claude-opus-4-8");
        let rs = rows(&same);
        let FieldVal::Inline(spans) = &rs.iter().find(|(l, _)| l == "model").unwrap().1 else {
            panic!("model is an inline value");
        };
        assert_eq!(spans.len(), 1, "an identical slug adds nothing");
    }

    /// Present extend-base / restrict paths carry the Path identity; absent
    /// ones read as a muted "none" rather than borrowing a hue.
    #[test]
    fn security_paths_are_roled_present_and_muted_when_absent() {
        let rs = rows(&sample("read-only"));
        assert_eq!(
            lead_role(&rs.iter().find(|(l, _)| l == "extend-base").unwrap().1),
            Some(Role::Muted),
            "absent extend-base is muted none"
        );

        let ext = PathBuf::from("/policy/base.ral");
        let restr = vec![PathBuf::from("src/lib.rs")];
        let mut s = sample("read-only");
        s.extend_base = Some(ext.as_path());
        s.restrict_files = &restr;
        let rs = rows(&s);
        assert_eq!(
            lead_role(&rs.iter().find(|(l, _)| l == "extend-base").unwrap().1),
            Some(Role::Path)
        );
        assert_eq!(
            lead_role(&rs.iter().find(|(l, _)| l == "restrict").unwrap().1),
            Some(Role::Path)
        );
    }

    /// The Bertin claim: rendered, every value lands in one shared column —
    /// each field line opens with a label cell of identical width.
    #[test]
    fn rendered_matrix_aligns_every_value_in_one_column() {
        let card = session_card(&sample("dangerous"));
        let lines = line::render_card(&card, 3);
        let label_w = rows(&sample("dangerous"))
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap()
            + 2;
        for l in &lines {
            // Dump so the column is eyeballable under `--nocapture`.
            eprintln!(
                "[{:>2}] {}",
                l.spans.first().map_or(0, |s| s.content.chars().count()),
                line::plain(l)
            );
        }
        for l in &lines {
            let Some(first) = l.spans.first() else {
                continue;
            };
            assert_eq!(
                first.content.chars().count(),
                label_w,
                "every field line opens with a label cell of width {label_w}"
            );
        }
    }

    /// The legend enumerates the rail's *own* shape vocabulary: every
    /// `RAIL_SHAPES` entry's name appears as a row label, so a new shape
    /// cannot land on the rail without showing up in the legend.
    #[test]
    fn legend_names_every_rail_shape() {
        let text: String = legend_panel()
            .iter()
            .map(line::plain)
            .collect::<Vec<_>>()
            .join("\n");
        for (_, name) in rail::RAIL_SHAPES {
            assert!(text.contains(name), "legend omits the {name:?} shape row");
        }
    }

    /// The legend is ambient, rail-less chrome: no row borrows a marginal
    /// rail glyph as its leading span.  The shape samples *contain* the
    /// glyphs, but always in a value cell behind a label — never as the
    /// row-leading rail the copy contract ([`line::plain`]) would strip.
    #[test]
    fn legend_wears_no_marginal_rail() {
        for l in legend_panel() {
            if let Some(first) = l.spans.first() {
                assert!(
                    !line::RAIL_GLYPHS.contains(&first.content.as_ref()),
                    "a legend row leads with a rail glyph: {:?}",
                    line::plain(&l)
                );
            }
        }
    }
}

#[cfg(test)]
mod command_tests {
    use super::{lookup_command, resolve_export_path};

    /// The matched command's canonical name plus the argument
    /// `lookup_command` peeled off — `None` when nothing matched.
    fn dispatch(input: &str) -> Option<(&'static str, String)> {
        lookup_command(input).map(|(c, arg)| (c.name, arg.to_string()))
    }

    #[test]
    fn argless_command_matches_alone_but_not_with_trailing_text() {
        assert_eq!(dispatch("/copy"), Some(("/copy", String::new())));
        // Trailing text on an argument-less command is not that command: it
        // falls through to the model as a prompt rather than running /copy.
        assert_eq!(dispatch("/copy this"), None);
        // An alias resolves to its canonical entry.
        assert_eq!(dispatch("/exit"), Some(("/quit", String::new())));
    }

    #[test]
    fn export_consumes_its_path_argument() {
        assert_eq!(
            dispatch("/export ~/notes.md"),
            Some(("/export", "~/notes.md".to_string()))
        );
        // Whitespace around the argument is trimmed.
        assert_eq!(
            dispatch("/export   /tmp/a.txt  "),
            Some(("/export", "/tmp/a.txt".to_string()))
        );
        // A bare /export still matches, with the empty argument its handler
        // turns into the usage hint.
        assert_eq!(dispatch("/export"), Some(("/export", String::new())));
    }

    #[test]
    fn unknown_token_is_not_a_command() {
        assert_eq!(dispatch("/bogus"), None);
        assert_eq!(dispatch("just a prompt"), None);
    }

    #[test]
    fn export_path_resolves_absolute_and_relative() {
        // An absolute path passes through (dots folded, cwd ignored).
        assert_eq!(
            resolve_export_path("/tmp/out.txt", "/Users/me/proj").to_str(),
            Some("/tmp/out.txt")
        );
        // A relative path anchors at the launch cwd, not the process cwd.
        assert_eq!(
            resolve_export_path("notes.md", "/Users/me/proj").to_str(),
            Some("/Users/me/proj/notes.md")
        );
    }
}
