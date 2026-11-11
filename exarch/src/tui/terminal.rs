use std::fs::File;
use std::io::{self, Stdout, Write as _};
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::process::Stdio;
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

fn run_editor(
    command: &EditorCommand,
    stderr: Stdio,
    draft: &DraftFile,
) -> io::Result<Result<String, String>> {
    let terminal = EditorTerminal::suspend();
    let mut cmd = site!(
        Silent::EditorCompose,
        std::process::Command::new(command.program())
    );
    cmd.args(command.args()).arg(draft.path()).stderr(stderr);
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
    // After `term`: field order restores stderr last.
    stderr: StderrRedirect,
}

impl TerminalGuard {
    /// Redirect fd 2 to `stderr.log` in `run`, then enter the terminal modes.
    ///
    /// # Errors
    /// If saving stderr, opening the log, redirecting to it, or entering
    /// terminal modes fails; nothing is left half-done.
    pub fn enter(run: &OwnDir) -> io::Result<Self> {
        let stderr = redirect_stderr_to_file(|| run.append(STDERR_LOG).map_err(io::Error::from))?;
        install_panic_restore_hook();
        TUI_ACTIVE.store(true, Ordering::Release);
        let term = enter_terminal_modes().inspect_err(|_| {
            restore_terminal_modes();
            TUI_ACTIVE.store(false, Ordering::Release);
        })?;
        Ok(Self { term, stderr })
    }

    /// Outer errors mean terminal restoration failed. Inner errors leave the input
    /// unchanged and must be displayed by the caller. Successful text is never submitted.
    pub(super) fn edit_text_in_editor(&self, text: &str) -> io::Result<Result<String, String>> {
        match self.prepare_edit(text) {
            Ok((command, stderr, draft)) => draft.edit(|draft| run_editor(&command, stderr, draft)),
            Err(diagnostic) => Ok(Err(diagnostic)),
        }
    }

    /// Prepare stdio before suspending modes, which never restores parent stderr.
    /// The draft comes last: nothing may fail before [`DraftFile::edit`] owns it.
    fn prepare_edit(&self, text: &str) -> Result<(EditorCommand, Stdio, DraftFile), String> {
        let command = editor_command()?;
        let drafts = draft_dir()?;
        let stderr = self
            .stderr
            .editor_stderr()
            .map_err(|e| format!("could not prepare saved stderr for the external editor: {e}"))?;
        let draft = DraftFile::create(drafts, text)?;
        Ok((command, stderr, draft))
    }

    pub fn term(&mut self) -> &mut Term {
        &mut self.term
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal_modes();
        TUI_ACTIVE.store(false, Ordering::Release);
    }
}

/// fd 2 aliased to a log until drop restores the saved original.  The saved
/// original is cloexec so it never leaks into a child; the redirect itself is
/// inherited on purpose, so a re-execed sandbox helper's traces join the log.
#[cfg(unix)]
struct StderrRedirect {
    saved: std::os::fd::OwnedFd,
}

/// Capture fd 2, then open the log with `open_log` and alias it onto fd 2.
#[cfg(unix)]
fn redirect_stderr_to_file(
    open_log: impl FnOnce() -> io::Result<File>,
) -> io::Result<StderrRedirect> {
    use std::os::fd::AsFd as _;

    let saved =
        rustix::io::fcntl_dupfd_cloexec(rustix::stdio::stderr(), 0).map_err(io::Error::from)?;
    let log = open_log()?;
    // SAFETY: fd 2 is a live process-global slot, all `clobber_slot` asks.
    unsafe { ral_core::process::clobber_slot(log.as_fd(), rustix::stdio::raw_stderr()) }?;
    Ok(StderrRedirect { saved })
}

#[cfg(unix)]
impl StderrRedirect {
    /// A fresh cloexec duplicate of the saved original, for one child.
    fn editor_stderr(&self) -> io::Result<Stdio> {
        rustix::io::fcntl_dupfd_cloexec(&self.saved, 0)
            .map(Stdio::from)
            .map_err(io::Error::from)
    }
}

/// Best-effort: a failure inside the TUI's drop has nowhere useful to surface.
#[cfg(unix)]
impl Drop for StderrRedirect {
    fn drop(&mut self) {
        use std::os::fd::AsFd as _;

        // SAFETY: fd 2 is a live process-global slot, all `clobber_slot` asks.
        let _ = unsafe {
            ral_core::process::clobber_slot(self.saved.as_fd(), rustix::stdio::raw_stderr())
        };
    }
}

/// Windows has no single fd table to `dup2` onto, so both stderr slots are
/// redirected: `STD_ERROR_HANDLE`, which `std::io::stderr` reads on every write
/// and `Stdio::inherit()` children receive, and CRT fd 2, which linked C uses.
///
/// `SetStdHandle` stores `log` without owning it, so the redirect owns `log`
/// until its drop has restored the slot.
#[cfg(windows)]
struct StderrRedirect {
    /// `None` when the original slot was absent or stale.
    saved_std: Option<std::os::windows::io::OwnedHandle>,
    saved_crt_fd: libc::c_int,
    log: std::os::windows::io::OwnedHandle,
}

/// Capture both slots, then open the log with `open_log` and redirect them.
/// Capture comes first: even the log's open could reuse a stale handle value.
#[cfg(windows)]
fn redirect_stderr_to_file(
    open_log: impl FnOnce() -> io::Result<File>,
) -> io::Result<StderrRedirect> {
    use std::os::windows::io::{AsRawHandle as _, IntoRawHandle as _, OwnedHandle};
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};

    let saved_std = snapshot_std_stderr()?;
    let log = OwnedHandle::from(open_log()?);
    // `_open_osfhandle` gives its handle to the fd it creates; closing that fd
    // must not close the handle `SetStdHandle` holds.
    let crt_handle = log.try_clone()?;
    // SAFETY: `crt_handle` is live; the CRT adopts it only on success.
    // `O_BINARY` stops the CRT rewriting `\n` against Rust's raw writes.
    let crt_fd = unsafe {
        libc::open_osfhandle(
            crt_handle.as_raw_handle() as isize,
            libc::O_APPEND | libc::O_BINARY,
        )
    };
    if crt_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let _adopted_by_crt = crt_handle.into_raw_handle();

    // SAFETY: fd 2 is always a valid CRT fd in a normal process.
    let saved_crt_fd = unsafe { libc::dup(2) };
    if saved_crt_fd < 0 {
        let e = io::Error::last_os_error();
        // SAFETY: `crt_fd` is this call's own.
        unsafe { libc::close(crt_fd) };
        return Err(e);
    }
    // SAFETY: both fds are live; the CRT analogue of the Unix `clobber_slot`.
    let r = unsafe { libc::dup2(crt_fd, 2) };
    let e = (r < 0).then(io::Error::last_os_error);
    // SAFETY: `crt_fd` is this call's own; on success fd 2 holds a duplicate.
    unsafe { libc::close(crt_fd) };
    if let Some(e) = e {
        // SAFETY: `saved_crt_fd` is this call's own.
        unsafe { libc::close(saved_crt_fd) };
        return Err(e);
    }

    let redirect = StderrRedirect {
        saved_std,
        saved_crt_fd,
        log,
    };
    // SAFETY: `redirect.log` stays open until the slot is restored.
    if unsafe { SetStdHandle(STD_ERROR_HANDLE, redirect.log.as_raw_handle()) } == 0 {
        // Dropping `redirect` undoes the CRT redirect.
        return Err(io::Error::last_os_error());
    }
    Ok(redirect)
}

/// An owned noninheritable duplicate of `STD_ERROR_HANDLE`, or `None` when the
/// slot is absent or stale.
#[cfg(windows)]
fn snapshot_std_stderr() -> io::Result<Option<std::os::windows::io::OwnedHandle>> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_INVALID_HANDLE, FALSE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: reading the standard-handle slot does not dereference its value.
    let original = unsafe { GetStdHandle(STD_ERROR_HANDLE) };
    if original.is_null() || original == INVALID_HANDLE_VALUE {
        return Ok(None);
    }
    let mut duplicate = std::ptr::null_mut();
    // SAFETY: DuplicateHandle validates even a stale original; only success
    // creates an owned handle. Neither handle is made inheritable here.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            original,
            GetCurrentProcess(),
            &raw mut duplicate,
            0,
            FALSE,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_INVALID_HANDLE.cast_signed()) {
            return Ok(None);
        }
        return Err(io::Error::new(
            error.kind(),
            format!("could not save pre-TUI stderr: {error}"),
        ));
    }
    // SAFETY: DuplicateHandle succeeded; this new noninheritable handle is ours.
    Ok(Some(unsafe { OwnedHandle::from_raw_handle(duplicate) }))
}

#[cfg(windows)]
impl StderrRedirect {
    /// A fresh duplicate of the saved original for one child, or discard when
    /// there was none.
    fn editor_stderr(&self) -> io::Result<Stdio> {
        self.saved_std
            .as_ref()
            .map_or_else(|| Ok(Stdio::null()), |h| h.try_clone().map(Stdio::from))
    }
}

/// Best-effort: a failure inside the TUI's drop has nowhere useful to surface.
#[cfg(windows)]
impl Drop for StderrRedirect {
    fn drop(&mut self) {
        use std::os::windows::io::IntoRawHandle;
        use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};

        // SAFETY: `saved_crt_fd` is this redirect's own `_dup` of fd 2.
        unsafe {
            libc::dup2(self.saved_crt_fd, 2);
            libc::close(self.saved_crt_fd);
        }
        // After `_dup2`, which also rewrites the slot. `SetStdHandle` does not
        // take ownership, so the snapshot is leaked into the slot.
        let saved = self
            .saved_std
            .take()
            .map_or(std::ptr::null_mut(), IntoRawHandle::into_raw_handle);
        // SAFETY: `saved` is null or a live handle nothing else owns.
        unsafe { SetStdHandle(STD_ERROR_HANDLE, saved) };
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::{DraftFile, editor_result};
    use ral_core::external_editor::MAX_DRAFT_BYTES;
    use ral_core::own::OwnDir;

    mod stderr;

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
