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
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend};

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
    // An ANSI terminal that does not implement it merely ignores the sequence,
    // but the *legacy Windows console API* (conhost) instead makes the push
    // itself fail — `PushKeyboardEnhancementFlags` returns "not implemented for
    // the legacy Windows API", which would abort terminal setup and take the
    // whole TUI down on startup. So gate the push on a support probe: where the
    // protocol is unsupported, skip it and degrade gracefully (losing only the
    // Meta/Alt chords) rather than refusing to open. The matching pop in
    // `restore_terminal_modes` is gated on `KBD_ENHANCED`, so a skipped push
    // stays balanced.
    if supports_keyboard_enhancement().unwrap_or(false) {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        KBD_ENHANCED.store(true, Ordering::Release);
    }
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
    // Each step below runs as its own `execute!` call rather than one batched
    // call: `execute!`/`queue!` chain commands with `.and_then`, so a single
    // failing write would abort every later command in the same batch.
    // conhost supports neither the kitty keyboard protocol nor DECSET 1003
    // and just ignores the bytes rather than erroring, but a batch is not
    // the place to bet on that — keeping each step independent means one
    // terminal quirk can never leave the console stuck in the alternate
    // screen or still capturing the mouse.
    let _ = execute!(io::stdout(), Show);
    // Pop any-motion reporting (1003) before the rest of mouse capture,
    // balancing the raw enable in `apply_terminal_modes`.
    let _ = execute!(io::stdout(), Print("\x1b[?1003l"));
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// The editor spec used when neither `$VISUAL` nor `$EDITOR` is set: `vi` on
/// Unix, `notepad` on Windows (`vi` isn't installed there).
#[cfg(windows)]
pub(super) const DEFAULT_EDITOR: &str = "notepad";
#[cfg(not(windows))]
pub(super) const DEFAULT_EDITOR: &str = "vi";

/// Pick the editor spec: `visual` if non-blank, else `editor` if non-blank,
/// else `default`.  Pulled out of [`editor_command`] as a pure function of
/// its inputs so the fallback logic is unit-testable without touching the
/// process environment.
pub(super) fn pick_editor_spec(visual: Option<&str>, editor: Option<&str>, default: &str) -> String {
    visual
        .filter(|s| !s.trim().is_empty())
        .or_else(|| editor.filter(|s| !s.trim().is_empty()))
        .unwrap_or(default)
        .to_string()
}

/// Resolve the editor to launch for `C-x C-e`: `$VISUAL`, then `$EDITOR`,
/// then [`DEFAULT_EDITOR`].  The value is split on whitespace so a spec like
/// `emacsclient -t` or `code --wait` keeps its arguments.
pub(super) fn editor_command() -> (String, Vec<String>) {
    let visual = std::env::var("VISUAL").ok();
    let editor = std::env::var("EDITOR").ok();
    let spec = pick_editor_spec(visual.as_deref(), editor.as_deref(), DEFAULT_EDITOR);
    let mut parts = spec.split_whitespace().map(str::to_string);
    let program = parts
        .next()
        .expect("spec is non-empty and non-whitespace, so it has a first token");
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
    stderr_backup: Option<std::os::fd::OwnedFd>,
    #[cfg(windows)]
    stderr_backup: Option<WindowsStderrBackup>,
}

impl TerminalGuard {
    #[cfg_attr(not(any(unix, windows)), allow(unused_variables))]
    pub fn enter(stderr_log: &Path) -> io::Result<Self> {
        install_panic_restore_hook();
        TUI_ACTIVE.store(true, Ordering::Release);
        #[cfg(any(unix, windows))]
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
                #[cfg(any(unix, windows))]
                if let Some(backup) = stderr_backup.take() {
                    restore_stderr(backup);
                }
                return Err(e);
            }
        };
        Ok(Self {
            term,
            #[cfg(any(unix, windows))]
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
        #[cfg(any(unix, windows))]
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
pub(super) fn redirect_stderr_to_file(path: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use std::fs::OpenOptions;
    use std::os::fd::AsFd;

    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let backup = rustix::io::dup(rustix::stdio::stderr()).map_err(io::Error::from)?;
    // SAFETY: fd 2 is a process-global fd slot; retargeting it onto the
    // log file is exactly the redirect this function exists to perform.
    unsafe { ral_core::process::clobber_slot(file.as_fd(), rustix::stdio::raw_stderr()) }?;
    Ok(backup)
}

/// Restore fd 2 from the `dup` saved by [`redirect_stderr_to_file`].
/// Best-effort: any failure inside the TUI's drop has nowhere useful
/// to surface, and the process is about to return to the user's
/// shell anyway.
#[cfg(unix)]
#[allow(
    clippy::needless_pass_by_value,
    reason = "consuming the backup is the contract: fd 2 is restored from it, then it closes here"
)]
pub(super) fn restore_stderr(backup: std::os::fd::OwnedFd) {
    use std::os::fd::AsFd;

    let target = rustix::stdio::raw_stderr();
    // SAFETY: fd 2 is a process-global fd slot; retargeting it back onto
    // the backup is exactly the restore this function exists to perform.
    let _ = unsafe { ral_core::process::clobber_slot(backup.as_fd(), target) };
}

/// Everything [`restore_stderr`] needs to undo [`redirect_stderr_to_file`]'s
/// two redirections on Windows.
///
/// Unlike the Unix path, `SetStdHandle` does not duplicate the handle it is
/// given — it stores the pointer as-is — so `log_handle` must stay open and
/// owned for as long as the redirect is active; closing it out from under
/// `SetStdHandle` would leave `STD_ERROR_HANDLE` dangling. It is closed
/// exactly once, in `restore_stderr`.
#[cfg(windows)]
pub(super) struct WindowsStderrBackup {
    /// `STD_ERROR_HANDLE`'s value before the redirect, restored via
    /// `SetStdHandle`.  May be null/invalid if nothing had set it, which
    /// `SetStdHandle` tolerates on restore the same way it did on entry.
    backup_std_handle: windows_sys::Win32::Foundation::HANDLE,
    /// A `dup` of the CRT's fd 2 before the redirect, restored via `_dup2`.
    backup_crt_fd: libc::c_int,
    /// The log file's handle, installed as `STD_ERROR_HANDLE`; owned by
    /// this backup and closed on restore.
    log_handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: `HANDLE` is a raw pointer with no thread-affinity; this backup is
// only ever moved from the thread that opens the TUI to the thread that
// tears it down (never shared concurrently), the same pattern
// `core::process::signal::windows::GroupState` uses for the same reason.
#[cfg(windows)]
unsafe impl Send for WindowsStderrBackup {}

/// Windows counterpart of [`redirect_stderr_to_file`]: there is no `dup2`
/// onto a single kernel fd table, so two independent redirections are
/// needed, both undone by [`restore_stderr`].
///
/// `std::io::stderr` calls `GetStdHandle(STD_ERROR_HANDLE)` fresh on every
/// write rather than caching it, so `SetStdHandle` alone redirects every
/// `eprintln!` exarch itself writes (and the writes of any child spawned
/// with `Stdio::inherit()`, since that inherits the *current* std handle).
/// It does not touch the CRT's fd table, though: linked C code that writes
/// to fd 2 via `_write`/the `stderr` `FILE*` reaches the old target
/// regardless of `SetStdHandle` — so the CRT fd is redirected too, via
/// `_dup2` reached through `libc`'s Windows CRT bindings (`_dup`, `_dup2`,
/// `_open_osfhandle`), the same call the Unix path makes through the kernel
/// directly.  Both are restored on exit, mirroring
/// `TerminalGuard.stderr_backup`'s lifecycle exactly.
#[cfg(windows)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:stderr-log] opens the TUI debug log for fd-2 redirect; trace infra, not turn-time data I/O"
)]
pub(super) fn redirect_stderr_to_file(path: &Path) -> io::Result<WindowsStderrBackup> {
    use std::fs::OpenOptions;
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE, HANDLE,
    };
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, SetStdHandle};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let file = OpenOptions::new().create(true).append(true).open(path)?;
    // `into_raw_handle` hands the handle out without running `File`'s
    // `Drop` — see `WindowsStderrBackup`'s doc for why it must stay open.
    let log_handle = file.into_raw_handle() as HANDLE;

    // A second, independent handle to the same file for the CRT fd path:
    // `_open_osfhandle` (below) gives the handle it's passed to the CRT fd
    // it creates, which is closed — taking its handle with it — once
    // `_dup2` has duplicated it onto fd 2 and the temporary fd is no
    // longer needed.  Sharing `log_handle` for that would close the very
    // handle `SetStdHandle` is holding.
    let mut crt_handle: HANDLE = std::ptr::null_mut();
    // SAFETY: `log_handle` is open (just obtained above); `crt_handle` is
    // an out-param this call fills on success.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            log_handle,
            GetCurrentProcess(),
            &mut crt_handle,
            0,
            FALSE,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        let e = io::Error::last_os_error();
        // SAFETY: `log_handle` is a handle this call owns and hasn't
        // handed off yet.
        unsafe {
            CloseHandle(log_handle);
        }
        return Err(e);
    }

    // SAFETY: `crt_handle` is a valid, freshly duplicated handle owned by
    // this call; `_open_osfhandle` adopts it into the CRT fd table, so it
    // must not be `CloseHandle`d directly after this succeeds.  `O_BINARY`
    // keeps the CRT fd's writes byte-for-byte: without it the CRT layer
    // translates `\n` to `\r\n`, so C writers and Rust writers (which go
    // straight to the handle, untranslated) would disagree on the log's
    // bytes.
    let crt_fd =
        unsafe { libc::open_osfhandle(crt_handle as isize, libc::O_APPEND | libc::O_BINARY) };
    if crt_fd < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: `_open_osfhandle` failed, so `crt_handle` was never
        // adopted and is still this call's to close; `log_handle` likewise.
        unsafe {
            CloseHandle(crt_handle);
            CloseHandle(log_handle);
        }
        return Err(e);
    }

    // SAFETY: fd 2 is always a valid CRT fd in a normal process; `_dup`
    // either returns a new fd or `-1` with errno set.
    let backup_crt_fd = unsafe { libc::dup(2) };
    if backup_crt_fd < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: `crt_fd` is this call's to close; doing so also closes
        // the `crt_handle` it adopted.  `log_handle` is still ours too.
        unsafe {
            libc::close(crt_fd);
            CloseHandle(log_handle);
        }
        return Err(e);
    }
    // SAFETY: `crt_fd` is open for the duration of this call; `_dup2`
    // atomically closes fd 2's existing CRT entry and duplicates `crt_fd`
    // onto it, so the two hold independent handles to the same file
    // afterwards — the CRT analogue of the Unix `dup2` call above.
    let r = unsafe { libc::dup2(crt_fd, 2) };
    if r < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: `backup_crt_fd`/`crt_fd` are both this call's to close;
        // `dup2` did not take ownership of either on failure.
        unsafe {
            libc::close(backup_crt_fd);
            libc::close(crt_fd);
            CloseHandle(log_handle);
        }
        return Err(e);
    }
    // SAFETY: `_dup2` already duplicated `crt_fd`'s handle onto fd 2;
    // closing the temporary fd does not affect fd 2's own copy.
    unsafe {
        libc::close(crt_fd);
    }

    // SAFETY: `GetStdHandle`/`SetStdHandle` take no invariant beyond the
    // named constant; a null/invalid `backup_std_handle` is a legitimate
    // "nothing was set" reading that `restore_stderr` passes straight back
    // to `SetStdHandle`, exactly as `GetStdHandle` returned it.
    let backup_std_handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if unsafe { SetStdHandle(STD_ERROR_HANDLE, log_handle) } == 0 {
        let e = io::Error::last_os_error();
        // SAFETY: undo the CRT-fd redirect and release `log_handle`, which
        // `SetStdHandle` never adopted since the call failed.
        unsafe {
            libc::dup2(backup_crt_fd, 2);
            libc::close(backup_crt_fd);
            CloseHandle(log_handle);
        }
        return Err(e);
    }

    Ok(WindowsStderrBackup {
        backup_std_handle,
        backup_crt_fd,
        log_handle,
    })
}

/// Restore both redirections made by [`redirect_stderr_to_file`]: the CRT
/// fd via `_dup2` (mirroring the Unix restore), then `STD_ERROR_HANDLE` via
/// `SetStdHandle`, then close the log handle this backup owned. Best-effort,
/// same rationale as the Unix restore: any failure inside the TUI's drop
/// has nowhere useful to surface.
#[cfg(windows)]
pub(super) fn restore_stderr(backup: WindowsStderrBackup) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};

    // SAFETY: `backup.backup_crt_fd` is a live fd from `_dup`; `_dup2` is
    // idempotent on the target and `_close` releases the backup — same
    // shape as the Unix restore.
    unsafe {
        libc::dup2(backup.backup_crt_fd, 2);
        libc::close(backup.backup_crt_fd);
    }
    // SAFETY: `backup.backup_std_handle` is whatever `GetStdHandle` held
    // before the redirect (possibly null/invalid, which `SetStdHandle`
    // tolerates); `backup.log_handle` is this backup's own handle, valid
    // until the `CloseHandle` below.
    unsafe {
        SetStdHandle(STD_ERROR_HANDLE, backup.backup_std_handle);
        CloseHandle(backup.log_handle);
    }
}

/// Raw-byte ceiling for an OSC 52 yank: base64-expanded (3→4 bytes) this
/// stays under the tightest common per-sequence cap (kitty's 8 KiB), so
/// the terminal accepts rather than silently drops the sequence.
pub(super) const YANK_CAP: usize = 6000;

/// Emit `text` to the host terminal's system clipboard via OSC 52.
///
/// Builds the sequence with `ral_core::ansi::osc52_copy` (see its doc
/// for the BEL-vs-ST terminator rationale); this function only owns
/// the base64 encoding and the write to stdout.  Terminals impose
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
    let sequence = ral_core::ansi::osc52_copy(&payload);
    let mut out = io::stdout().lock();
    out.write_all(sequence.as_bytes())?;
    out.flush()
}

/// The last `cap` bytes of `text`, snapped forward to the nearest char
/// boundary so the slice is always valid UTF-8.  Returns all of `text`
/// when it already fits.
pub(super) fn tail_bytes(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let start = ral_core::text::ceil_char_boundary(text, text.len() - cap);
    &text[start..]
}

#[cfg(test)]
mod tests {
    //! [`pick_editor_spec`] is a plain function of its three string
    //! arguments, so the `$VISUAL`/`$EDITOR`/default fallback logic is
    //! exercised here without touching the process environment or the
    //! platform default (`DEFAULT_EDITOR` is passed in, not read).

    use super::pick_editor_spec;

    #[test]
    fn visual_wins_when_set_and_non_blank() {
        assert_eq!(
            pick_editor_spec(Some("emacsclient -t"), Some("nano"), "vi"),
            "emacsclient -t"
        );
    }

    #[test]
    fn editor_wins_when_visual_is_unset_or_blank() {
        assert_eq!(pick_editor_spec(None, Some("nano"), "vi"), "nano");
        assert_eq!(pick_editor_spec(Some("   "), Some("nano"), "vi"), "nano");
    }

    #[test]
    fn default_wins_when_both_are_unset_or_blank() {
        assert_eq!(pick_editor_spec(None, None, "vi"), "vi");
        assert_eq!(pick_editor_spec(Some(""), Some("  "), "notepad"), "notepad");
    }
}
