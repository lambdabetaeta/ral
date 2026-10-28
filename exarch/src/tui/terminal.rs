use std::fs::File;
use std::io::{self, Stdout, Write as _};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
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
use ral_core::external_editor::{EditorCommand, admit_draft, after_cleanup, read_draft};
use ral_core::own::OwnDir;
use ral_core::path::{Visibility, XdgKind};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::app::EXARCH;
use crate::site::{Silent, site};

/// The TUI's debug log, in the run's directory: what fd 2 is redirected to.
const STDERR_LOG: &str = "stderr.log";

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

/// Drafts live in exarch's cache, beside its scratch subtree, never inside it:
/// the built-in grants leave the cache read-only and only scratch writable.
fn draft_dir() -> Result<OwnDir, String> {
    EXARCH
        .own(XdgKind::Cache)
        .and_then(|cache| cache.make_dir("editor-drafts"))
        .map_err(|e| format!("could not open editor draft directory: {e}"))
}

fn editor_command() -> Result<EditorCommand, String> {
    EditorCommand::select(
        ral_core::host::var("VISUAL").as_deref(),
        ral_core::host::var("EDITOR").as_deref(),
    )
    .map_err(|e| format!("cannot run the external editor: {e}"))
}

/// A secret draft file.  [`Self::remove`] consumes it, so it is attempted once.
struct DraftFile {
    dir: OwnDir,
    name: String,
}

impl DraftFile {
    /// Written whole, or removed again before the error returns.
    fn create(dir: OwnDir, text: &str) -> Result<Self, String> {
        let bytes = admit_draft(text).map_err(|e| format!("could not write editor draft: {e}"))?;
        let (name, mut file) = dir
            .create_fresh("prompt-", ".md", Visibility::Secret)
            .map_err(|e| format!("could not create secret editor draft: {e}"))?;
        let draft = Self { dir, name };
        let written = file
            .write_all(bytes)
            .map_err(|e| format!("could not write editor draft: {e}"));
        drop(file);
        match written {
            Ok(()) => Ok(draft),
            Err(error) => after_cleanup(Err(error), draft.remove(), joined),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join(&self.name)
    }

    fn read(&self) -> Result<String, String> {
        let file = self
            .dir
            .open(&self.name)
            .map_err(|e| format!("could not read editor draft: {e}"))?;
        read_draft(file).map_err(|e| format!("could not read editor draft: {e}"))
    }

    fn remove(self) -> Result<(), String> {
        self.dir.remove(&self.name).map_err(|e| {
            format!(
                "could not remove secret editor draft at {}: {e}; remove it manually",
                ral_core::path::shown(self.path()),
            )
        })
    }

    /// Lend the draft to `edit`, then remove it: whether `edit` returns, fails
    /// to restore the terminal, or unwinds.
    fn edit<T>(
        self,
        edit: impl FnOnce(&Self) -> io::Result<Result<T, String>>,
    ) -> io::Result<Result<T, String>> {
        let edited = panic::catch_unwind(AssertUnwindSafe(|| edit(&self)));
        let removed = self.remove();
        match edited {
            Ok(Ok(edited)) => Ok(after_cleanup(edited, removed, joined)),
            Ok(Err(terminal)) => Err(match removed {
                Ok(()) => terminal,
                Err(removal) => {
                    io::Error::new(terminal.kind(), joined(terminal.to_string(), removal))
                }
            }),
            Err(payload) => {
                if let Err(removal) = removed {
                    ral_core::errln!("{removal}");
                }
                panic::resume_unwind(payload)
            }
        }
    }
}

fn joined(first: String, second: String) -> String {
    [first, second].join("; ")
}

/// Unwinding must leave a cooked terminal, not re-enter the TUI after its hook.
struct EditorTerminal;

impl EditorTerminal {
    fn suspend() -> Self {
        restore_terminal_modes();
        Self
    }

    fn resume(self) -> io::Result<()> {
        apply_terminal_modes()?;
        std::mem::forget(self);
        Ok(())
    }
}

impl Drop for EditorTerminal {
    fn drop(&mut self) {
        restore_terminal_modes();
    }
}

fn editor_stderr() -> io::Result<File> {
    #[cfg(unix)]
    let path = "/dev/tty";
    #[cfg(windows)]
    let path = "CONOUT$";
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    site!(Silent::EditorCompose, options => open(path))
}

/// Outer errors mean terminal restoration failed. Inner errors leave the input
/// unchanged and must be displayed by the caller. Successful text is never submitted.
pub(super) fn edit_text_in_editor(text: &str) -> io::Result<Result<String, String>> {
    match prepare_edit(text) {
        Ok((command, stderr, draft)) => draft.edit(|draft| run_editor(&command, stderr, draft)),
        Err(diagnostic) => Ok(Err(diagnostic)),
    }
}

/// Everything fallible before the terminal is suspended.  The draft comes
/// last: nothing may fail between its creation and [`DraftFile::edit`].
fn prepare_edit(text: &str) -> Result<(EditorCommand, File, DraftFile), String> {
    let command = editor_command()?;
    let drafts = draft_dir()?;
    let stderr = editor_stderr()
        .map_err(|e| format!("could not open terminal for editor diagnostics: {e}"))?;
    let draft = DraftFile::create(drafts, text)?;
    Ok((command, stderr, draft))
}

fn run_editor(
    command: &EditorCommand,
    stderr: File,
    draft: &DraftFile,
) -> io::Result<Result<String, String>> {
    let terminal = EditorTerminal::suspend();
    let mut cmd = site!(
        Silent::EditorCompose,
        std::process::Command::new(command.program())
    );
    cmd.args(command.args())
        .arg(draft.path())
        .stderr(std::process::Stdio::from(stderr));
    let status = ral_core::process::status(&mut cmd);
    terminal.resume()?;
    Ok(editor_result(command.program(), status, draft))
}

fn editor_result(
    program: &str,
    status: io::Result<std::process::ExitStatus>,
    draft: &DraftFile,
) -> Result<String, String> {
    match status {
        Ok(status) if status.success() => draft.read(),
        Ok(status) => Err(format!(
            "editor {program:?} exited with {status}; draft unchanged"
        )),
        Err(error) => Err(format!(
            "could not run editor {program:?}: {error}; draft unchanged"
        )),
    }
}

/// RAII guard over the terminal modes and a redirect of fd 2.
///
/// While it is live, fd 2 goes to a per-process log so `dbg_trace!` cannot
/// tear through the rendered frame. `Drop` undoes both, so unwinding cannot
/// skip the cleanup.
pub struct TerminalGuard {
    term: Term,
    #[cfg(unix)]
    stderr_backup: Option<std::os::fd::OwnedFd>,
    #[cfg(windows)]
    stderr_backup: Option<WindowsStderrBackup>,
}

impl TerminalGuard {
    /// Redirect fd 2 to `stderr.log` in `run`, then enter the terminal modes.
    ///
    /// # Errors
    /// If the log cannot be opened or the terminal cannot enter its modes;
    /// either way nothing is left half-done.
    pub fn enter(run: &OwnDir) -> io::Result<Self> {
        let stderr_log = run.append(STDERR_LOG)?;
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

/// Alias `file` onto fd 2, returning a cloexec backup of the original.  The
/// backup must not leak into a child; the redirect itself is inherited on
/// purpose, so a re-execed sandbox helper's traces join the same log.
#[cfg(unix)]
#[allow(
    clippy::needless_pass_by_value,
    reason = "the redirect takes the file whole on every host: Windows hands its handle on, and Unix closes it here once fd 2 holds the log"
)]
pub(super) fn redirect_stderr_to_file(file: File) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::AsFd;

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
pub(super) fn redirect_stderr_to_file(file: File) -> io::Result<WindowsStderrBackup> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, FALSE, HANDLE,
    };
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, SetStdHandle};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

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

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::{DraftFile, editor_result};
    use ral_core::external_editor::MAX_DRAFT_BYTES;
    use ral_core::own::OwnDir;

    #[test]
    fn a_draft_is_lossless_secret_and_removed_after_its_edit() {
        let dir = OwnDir::for_test("editor-draft").unwrap();
        let draft = DraftFile::create((*dir).clone(), "one\r\ntwo\n\n").unwrap();
        let path = draft.path();
        assert_eq!(draft.read().unwrap(), "one\r\ntwo\n\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(
            draft
                .edit(|draft| Ok(draft.read().map(drop)))
                .unwrap()
                .is_ok()
        );
        assert!(!path.exists());
    }

    #[test]
    fn an_unwinding_edit_still_removes_its_draft() {
        let dir = OwnDir::for_test("editor-unwind").unwrap();
        let draft = DraftFile::create((*dir).clone(), "original").unwrap();
        let path = draft.path();
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            draft.edit(|_| -> std::io::Result<Result<(), String>> { panic!("editor") })
        }));
        assert!(unwound.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn a_failed_terminal_restore_still_removes_the_draft() {
        let dir = OwnDir::for_test("editor-restore").unwrap();
        let draft = DraftFile::create((*dir).clone(), "original").unwrap();
        let path = draft.path();
        let error = draft
            .edit(|_| -> std::io::Result<Result<(), String>> {
                Err(std::io::Error::other("no tty"))
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "no tty");
        assert!(!path.exists());
    }

    #[test]
    fn readback_is_bounded_and_the_draft_still_removed() {
        let dir = OwnDir::for_test("editor-bounds").unwrap();
        let draft = DraftFile::create((*dir).clone(), "original").unwrap();
        let path = draft.path();
        dir.open_write(&draft.name)
            .unwrap()
            .set_len((MAX_DRAFT_BYTES + 1) as u64)
            .unwrap();
        let error = draft
            .edit(|draft| Ok(draft.read().map(drop)))
            .unwrap()
            .unwrap_err();
        assert!(error.contains("16 MiB limit"), "{error}");
        assert!(!path.exists());
        assert!(
            DraftFile::create((*dir).clone(), &"x".repeat(MAX_DRAFT_BYTES + 1))
                .err()
                .unwrap()
                .contains("16 MiB limit")
        );
    }

    #[cfg(unix)]
    #[test]
    fn readback_refuses_symlinks_and_removal_does_not_follow_them() {
        let dir = OwnDir::for_test("editor-link").unwrap();
        let draft = DraftFile::create((*dir).clone(), "original").unwrap();
        let path = draft.path();
        dir.remove(&draft.name).unwrap();
        std::os::unix::fs::symlink("nonexistent-target", &path).unwrap();
        assert!(draft.read().unwrap_err().contains("could not read"));
        draft.remove().unwrap();
        assert!(std::fs::symlink_metadata(path).is_err());
    }

    #[test]
    fn spawn_and_read_errors_are_recoverable_diagnostics() {
        let dir = OwnDir::for_test("editor-errors").unwrap();
        let draft = DraftFile::create((*dir).clone(), "original").unwrap();
        let error = editor_result(
            "missing-editor",
            Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            &draft,
        )
        .unwrap_err();
        assert!(error.contains("missing-editor"));
        assert!(error.contains("draft unchanged"));
        assert_eq!(draft.read().unwrap(), "original");
        std::fs::write(draft.path(), [0xff]).unwrap();
        assert!(draft.read().unwrap_err().contains("not UTF-8"));
        dir.remove(&draft.name).unwrap();
        assert!(draft.read().unwrap_err().contains("could not read"));
    }

    #[cfg(unix)]
    #[test]
    fn an_unsuccessful_editor_does_not_return_its_partial_edit() {
        use std::os::unix::process::ExitStatusExt as _;
        let dir = OwnDir::for_test("editor-cancel").unwrap();
        let draft = DraftFile::create((*dir).clone(), "partial edit").unwrap();
        for status in [
            std::process::ExitStatus::from_raw(256),
            std::process::ExitStatus::from_raw(2),
        ] {
            let error = editor_result("editor", Ok(status), &draft).unwrap_err();
            assert!(error.contains("draft unchanged"));
        }
        draft.remove().unwrap();
    }
}
