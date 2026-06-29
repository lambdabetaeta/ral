use std::io::{self, Stdout};
use std::path::Path;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::{
    cursor::Show,
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::Print,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen,
        disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
};

use super::tui_loop::Tui;

pub type Term = Terminal<CrosstermBackend<Stdout>>;

pub static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whether the kitty keyboard-enhancement flags are currently pushed, so the
/// matching pop runs exactly once even when the panic hook and `Drop` both
/// reach [`restore_terminal_modes`] on an unwind.
pub(super) static KBD_ENHANCED: AtomicBool = AtomicBool::new(false);
pub(super) static PANIC_RESTORE_HOOK: Once = Once::new();

pub(super) fn install_panic_restore_hook() {
    PANIC_RESTORE_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if TUI_ACTIVE.swap(false, Ordering::AcqRel) {
                restore_terminal_modes();
            }
            previous(info);
            // Flush stderr so the panic message reaches the log before
            // TerminalGuard::drop restores the fd — without this the
            // message sits in the file buffer and is lost on abort.
            use std::io::Write;
            let _ = std::io::stderr().flush();
        }));
    });
}

/// Apply the raw-mode + alternate-screen + bracketed-paste + mouse-capture
/// modes to the current `stdout`, and opt into the kitty keyboard protocol.
/// Split out from [`enter_terminal_modes`] so the editor hatch
/// ([`compose_in_editor`]) can re-enter the same modes after suspending the
/// TUI for a child editor, without building a second [`Term`].
pub(super) fn apply_terminal_modes() -> io::Result<()> {
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

pub fn enter_terminal_modes() -> io::Result<Term> {
    apply_terminal_modes()?;
    let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    term.hide_cursor()?;
    Ok(term)
}

pub(super) fn restore_terminal_modes() {
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
pub(super) fn editor_command() -> (String, Vec<String>) {
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
pub(super) fn edit_text_in_editor(draft: &str) -> io::Result<Option<String>> {
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
pub fn compose_in_editor(tui: &mut Tui) -> io::Result<()> {
    let draft = tui.app.prompt_state.prompt_text();
    if let Some(edited) = edit_text_in_editor(&draft)? {
        tui.app.prompt_state.adopt_draft(&edited);
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
pub(super) fn redirect_stderr_to_file(path: &Path) -> io::Result<std::os::fd::RawFd> {
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
pub(super) fn restore_stderr(backup: std::os::fd::RawFd) {
    // SAFETY: `backup` is a live fd returned by `dup`; `dup2` is
    // idempotent on the target and `close` releases the backup.
    unsafe {
        libc::dup2(backup, libc::STDERR_FILENO);
        libc::close(backup);
    }
}
/// Raw-byte ceiling for an OSC 52 yank: base64-expanded (3→4 bytes) this
/// stays under the tightest common per-sequence cap (kitty's 8 KiB), so
/// the terminal accepts rather than silently drops the sequence.
pub(super) const YANK_CAP: usize = 6000;

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
pub(super) fn osc52_copy(text: &str) -> io::Result<()> {
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
pub(super) fn tail_bytes(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}