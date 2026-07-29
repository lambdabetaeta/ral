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
/// Whether the kitty enhancement flags are pushed, so the pop runs exactly once
/// when the panic hook and `Drop` both reach `restore_terminal_modes`.
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
            // Flush before `TerminalGuard::drop` restores fd 2, or the panic
            // message dies in the log file's buffer on abort.
            use std::io::Write;
            let _ = std::io::stderr().flush();
        }));
    });
}

/// Apply the TUI's terminal modes to the current `stdout`.  Separate from
/// [`enter_terminal_modes`] so the `C-x C-e` hatch can re-enter them after
/// the child editor exits, without building a second [`Term`].
pub(super) fn apply_terminal_modes() -> io::Result<()> {
    enable_raw_mode()?;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture,
        // Any-motion reporting (DECSET 1003), so the hover dial follows a
        // pointer with no button held.  Crossterm has no typed command for it.
        Print("\x1b[?1003h"),
    )?;
    // Without the protocol the emacs keymap's Meta chords never reach crossterm
    // as ALT events.  A terminal lacking it ignores the sequence, but legacy
    // conhost fails the push itself, which would take startup down — so probe,
    // and lose the Meta chords rather than refuse to open.
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
    // One `execute!` per step: it chains commands with `.and_then`, so a single
    // failing write inside a batch would swallow every later one and could
    // strand the console in the alternate screen or still capturing the mouse.
    let _ = execute!(io::stdout(), Show);
    // Any-motion reporting off, balancing the raw enable in `apply_terminal_modes`.
    let _ = execute!(io::stdout(), Print("\x1b[?1003l"));
    let _ = execute!(io::stdout(), DisableMouseCapture);
    let _ = execute!(io::stdout(), DisableBracketedPaste);
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
}

/// The editor spec used when neither `$VISUAL` nor `$EDITOR` is set.
#[cfg(windows)]
pub(super) const DEFAULT_EDITOR: &str = "notepad";
#[cfg(not(windows))]
pub(super) const DEFAULT_EDITOR: &str = "vi";

/// The first non-blank of `visual`, `editor`, `default`.  Split out of
/// [`editor_command`] so the fallback is testable without the environment.
pub(super) fn pick_editor_spec(
    visual: Option<&str>,
    editor: Option<&str>,
    default: &str,
) -> String {
    visual
        .filter(|s| !s.trim().is_empty())
        .or_else(|| editor.filter(|s| !s.trim().is_empty()))
        .unwrap_or(default)
        .to_string()
}

/// Resolve the editor to launch for `C-x C-e`.  Split on whitespace so a spec
/// like `emacsclient -t` keeps its arguments.
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

/// Compose `draft` in the user's `$EDITOR`: scratch file, suspend the terminal
/// modes so the child owns the tty, run, re-enter, read back.  One trailing
/// newline is trimmed, the prompt being newline-joined.  `None` keeps the
/// original draft; only failure to re-enter the modes propagates.
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

/// Drive the `C-x C-e` hatch from the UI loop, which owns the terminal the
/// editor must borrow.  Repaints over whatever the editor left on the screen.
pub fn compose_in_editor(tui: &mut Tui) -> io::Result<()> {
    let draft = tui.app.prompt_state.prompt_text();
    if let Some(edited) = edit_text_in_editor(&draft)? {
        tui.app.prompt_state.adopt_draft(&edited);
    }
    tui.guard.term().clear()?;
    Ok(())
}

/// RAII guard over the terminal modes and, while it is live, a redirect of fd 2
/// to a per-process log so `dbg_trace!` cannot tear through the rendered frame.
/// `Drop` undoes both, so unwinding cannot skip the cleanup.
pub struct TerminalGuard {
    term: Term,
    #[cfg(unix)]
    stderr_backup: Option<std::os::fd::OwnedFd>,
    #[cfg(windows)]
    stderr_backup: Option<WindowsStderrBackup>,
}

impl TerminalGuard {
    pub fn enter(stderr_log: &Path) -> io::Result<Self> {
        install_panic_restore_hook();
        TUI_ACTIVE.store(true, Ordering::Release);
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
                if let Some(backup) = stderr_backup.take() {
                    restore_stderr(backup);
                }
                return Err(e);
            }
        };
        Ok(Self {
            term,
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
        // stderr last: teardown's own stray diagnostics still belong in the
        // log, not on the freshly-restored prompt row.
        if let Some(backup) = self.stderr_backup.take() {
            restore_stderr(backup);
        }
    }
}

/// Alias `path` onto fd 2, returning a cloexec backup of the original.  The
/// backup must not leak into a child; the redirect itself is inherited on
/// purpose, so a re-execed sandbox helper's traces join the same log.
#[cfg(unix)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:stderr-log] opens the TUI debug log for fd-2 redirect; trace infra, not turn-time data I/O"
)]
pub(super) fn redirect_stderr_to_file(path: &Path) -> io::Result<std::os::fd::OwnedFd> {
    use std::fs::OpenOptions;
    use std::os::fd::AsFd;

    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let backup =
        rustix::io::fcntl_dupfd_cloexec(rustix::stdio::stderr(), 0).map_err(io::Error::from)?;
    // SAFETY: fd 2 is a live process-global slot, all `clobber_slot` asks.
    unsafe { ral_core::process::clobber_slot(file.as_fd(), rustix::stdio::raw_stderr()) }?;
    Ok(backup)
}

/// Restore fd 2 from the backup [`redirect_stderr_to_file`] saved.  Best-effort:
/// a failure inside the TUI's drop has nowhere useful to surface.
#[cfg(unix)]
#[allow(
    clippy::needless_pass_by_value,
    reason = "consuming the backup is the contract: fd 2 is restored from it, then it closes here"
)]
pub(super) fn restore_stderr(backup: std::os::fd::OwnedFd) {
    use std::os::fd::AsFd;

    let target = rustix::stdio::raw_stderr();
    // SAFETY: fd 2 is a live process-global slot, all `clobber_slot` asks.
    let _ = unsafe { ral_core::process::clobber_slot(backup.as_fd(), target) };
}

/// Everything [`restore_stderr`] needs to undo [`redirect_stderr_to_file`]'s
/// two redirections on Windows.
///
/// `SetStdHandle` stores the pointer it is given rather than duplicating it, so
/// `log_handle` must stay open and owned for the life of the redirect or
/// `STD_ERROR_HANDLE` dangles.  It is closed exactly once, in `restore_stderr`.
#[cfg(windows)]
pub(super) struct WindowsStderrBackup {
    /// `STD_ERROR_HANDLE` before the redirect; may be null, which `SetStdHandle`
    /// tolerates on restore as it did on entry.
    backup_std_handle: windows_sys::Win32::Foundation::HANDLE,
    backup_crt_fd: libc::c_int,
    /// The log file, installed as `STD_ERROR_HANDLE`; closed on restore.
    log_handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: `HANDLE` is a raw pointer with no thread affinity, and this backup
// only ever moves from the thread that opens the TUI to the one that tears it
// down, never shared — as `GroupState` in `core/src/process/signal/windows.rs`.
#[cfg(windows)]
unsafe impl Send for WindowsStderrBackup {}

/// Windows counterpart of the Unix `redirect_stderr_to_file`: with no single
/// kernel fd table to `dup2` onto, two independent redirections are needed,
/// both undone by [`restore_stderr`].  `std::io::stderr` calls `GetStdHandle`
/// fresh on every write, so `SetStdHandle` alone catches every `eprintln!`
/// exarch writes and every child spawned with `Stdio::inherit()`; it leaves the
/// CRT fd table alone, which linked C code uses — hence the `_dup2` too.
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
    // Handed out without running `File`'s `Drop`; ownership passes to the
    // `WindowsStderrBackup`, whose doc says why it must stay open.
    let log_handle = file.into_raw_handle() as HANDLE;

    // A second, independent handle for the CRT path: `_open_osfhandle` gives its
    // handle to the fd it creates, and closing that temporary fd would close the
    // very handle `SetStdHandle` holds.
    let mut crt_handle: HANDLE = std::ptr::null_mut();
    // SAFETY: `log_handle` is open; `crt_handle` is an out-param filled on success.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            log_handle,
            GetCurrentProcess(),
            &raw mut crt_handle,
            0,
            FALSE,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        let e = io::Error::last_os_error();
        // SAFETY: `log_handle` is this call's, not yet handed off.
        unsafe {
            CloseHandle(log_handle);
        }
        return Err(e);
    }

    // SAFETY: `crt_handle` is freshly duplicated and owned here; the CRT adopts
    // it, so it must not be `CloseHandle`d once this succeeds.  `O_BINARY` stops
    // the CRT rewriting `\n`, which would disagree with Rust's untranslated
    // writes straight to the handle.
    let crt_fd =
        unsafe { libc::open_osfhandle(crt_handle as isize, libc::O_APPEND | libc::O_BINARY) };
    if crt_fd < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: the adoption failed, so `crt_handle` is still this call's to
        // close, as is `log_handle`.
        unsafe {
            CloseHandle(crt_handle);
            CloseHandle(log_handle);
        }
        return Err(e);
    }

    // SAFETY: fd 2 is always a valid CRT fd in a normal process.
    let backup_crt_fd = unsafe { libc::dup(2) };
    if backup_crt_fd < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: closing `crt_fd` also closes the `crt_handle` it adopted;
        // `log_handle` is still ours.
        unsafe {
            libc::close(crt_fd);
            CloseHandle(log_handle);
        }
        return Err(e);
    }
    // SAFETY: `crt_fd` is open here; `_dup2` closes fd 2's CRT entry and
    // duplicates onto it — the CRT analogue of the Unix path's `clobber_slot`.
    let r = unsafe { libc::dup2(crt_fd, 2) };
    if r < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: `_dup2` took ownership of nothing on failure, so both fds
        // and `log_handle` are still this call's to close.
        unsafe {
            libc::close(backup_crt_fd);
            libc::close(crt_fd);
            CloseHandle(log_handle);
        }
        return Err(e);
    }
    // SAFETY: fd 2 holds its own duplicate now, unaffected by this close.
    unsafe {
        libc::close(crt_fd);
    }

    // SAFETY: these take no invariant beyond the named constant; a null reading
    // is a legitimate "nothing was set" that `restore_stderr` hands straight back.
    let backup_std_handle = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if unsafe { SetStdHandle(STD_ERROR_HANDLE, log_handle) } == 0 {
        let e = io::Error::last_os_error();
        // SAFETY: `SetStdHandle` failed, so it never adopted `log_handle`;
        // undo the CRT-fd redirect and release it.
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

/// Undo both of [`redirect_stderr_to_file`]'s redirections, then close the log
/// handle this backup owned.  Best-effort: a failure inside the TUI's drop has
/// nowhere useful to surface.
#[cfg(windows)]
#[allow(
    clippy::needless_pass_by_value,
    reason = "consuming the backup is the contract: both redirections are undone from it, and the log handle it owns is closed here"
)]
pub(super) fn restore_stderr(backup: WindowsStderrBackup) {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};

    // SAFETY: `backup_crt_fd` is a live fd from `_dup`, duplicated back onto
    // fd 2 and then released.
    unsafe {
        libc::dup2(backup.backup_crt_fd, 2);
        libc::close(backup.backup_crt_fd);
    }
    // SAFETY: `backup_std_handle` is whatever `GetStdHandle` held, possibly null;
    // `log_handle` is this backup's own, valid until the `CloseHandle` below.
    unsafe {
        SetStdHandle(STD_ERROR_HANDLE, backup.backup_std_handle);
        CloseHandle(backup.log_handle);
    }
}

/// Raw-byte ceiling for an OSC 52 yank: base64 expands 3→4, keeping this under
/// the tightest common per-sequence cap, kitty's 8 KiB.
pub(super) const YANK_CAP: usize = 6000;

/// Emit `text` to the host terminal's clipboard via OSC 52.  Oversized sequences
/// are dropped in silence, so callers bound the slice (see [`YANK_CAP`]); tmux
/// needs `set-clipboard on`, and on 3.3+ `allow-passthrough on`, or it eats the
/// sequence on the way out.
pub(super) fn osc52_copy(text: &str) -> io::Result<()> {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use std::io::Write;
    let payload = STANDARD.encode(text);
    let sequence = ral_core::ansi::osc52_copy(&payload);
    let mut out = io::stdout().lock();
    out.write_all(sequence.as_bytes())?;
    out.flush()
}

/// The last `cap` bytes of `text`, snapped forward to a char boundary; all of
/// `text` when it already fits.
pub(super) fn tail_bytes(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let start = ral_core::text::ceil_char_boundary(text, text.len() - cap);
    &text[start..]
}

#[cfg(test)]
mod tests {
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
