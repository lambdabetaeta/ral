//! Platform compatibility shims.
//!
//! Provides TTY detection, ANSI virtual-terminal-processing setup on
//! Windows, and in-process stdio redirection for the embedded uutils
//! coreutils/diffutils shims.  On Unix the redirection uses `dup`/`dup2`;
//! on Windows it swaps the Win32 standard-handle slots directly.
//!
//! Pipeline stages may run on concurrent OS threads (see
//! `evaluator/pipeline/stages.rs`), and the redirection helpers manipulate
//! process-wide fd 0/1 — i.e. shared state.  Callers MUST hold the guard
//! returned by [`lock_stdio_redirect`] across any sequence that touches fd
//! 0 or fd 1, otherwise two concurrent redirects can interleave their
//! save/restore and route bytes into the wrong pipe.

/// Format a "command not found" message for `cmd`.
pub(crate) fn not_found_hint(cmd: &str) -> String {
    format!("{cmd}: command not found")
}

// ── Windows console: TTY detection and VTP setup ──────────────────────────
//
// GetConsoleMode succeeds only on real console handles, making it a reliable
// isatty substitute.  ENABLE_VIRTUAL_TERMINAL_PROCESSING must be set before
// any ANSI output — uutils (uu_ls etc.) emits escape codes but relies on the
// host process to have switched the console into VTP mode first.

#[cfg(windows)]
pub const STD_INPUT_HANDLE: u32 = 0xFFFFFFF6; // (DWORD)(-10)
#[cfg(windows)]
pub const STD_OUTPUT_HANDLE: u32 = 0xFFFFFFF5; // (DWORD)(-11)
#[cfg(windows)]
pub const STD_ERROR_HANDLE: u32 = 0xFFFFFFF4; // (DWORD)(-12)

#[cfg(windows)]
unsafe extern "system" {
    fn GetStdHandle(nStdHandle: u32) -> *mut std::ffi::c_void;
    fn GetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, lpMode: *mut u32) -> i32;
    fn SetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, dwMode: u32) -> i32;
}

#[cfg(all(windows, any(feature = "coreutils", feature = "diffutils")))]
unsafe extern "system" {
    fn SetStdHandle(nStdHandle: u32, hHandle: *mut std::ffi::c_void) -> i32;
}

/// Returns true when the given Win32 standard-handle ID is attached to a
/// console (not a pipe, file, or NUL).  Used as `isatty` on Windows.
#[cfg(windows)]
pub fn is_console(std_handle: u32) -> bool {
    let h = unsafe { GetStdHandle(std_handle) };
    let mut mode: u32 = 0;
    unsafe { GetConsoleMode(h, &mut mode) != 0 }
}

/// Enable ANSI virtual-terminal processing on the stdout and stderr console
/// handles.  Must be called once at process startup.  A no-op when a handle
/// is redirected to a pipe or file (GetConsoleMode will fail on those).
#[cfg(windows)]
pub fn enable_virtual_terminal_processing() {
    const ENABLE_VTP: u32 = 0x0004;
    for id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let h = unsafe { GetStdHandle(id) };
        let mut mode: u32 = 0;
        if unsafe { GetConsoleMode(h, &mut mode) } != 0 {
            unsafe {
                SetConsoleMode(h, mode | ENABLE_VTP);
            }
        }
    }
}

// ── stdio redirect serialization ─────────────────────────────────────────
//
// dup2 of fd 0/1 mutates global process state.  When pipeline stages run on
// concurrent threads, two stages each calling `with_*_redirected` would
// interleave their save/restore and route writes into the wrong fd.  This
// mutex is taken by `lock_stdio_redirect` and held across the
// redirect/run/restore window.  The cost is that two in-process uutils
// stages within the same pipeline run sequentially under the lock — that
// is the price of dup2-based redirection.

#[cfg(any(feature = "coreutils", feature = "diffutils"))]
static STDIO_REDIRECT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the global stdio-redirect lock.
///
/// Hold the returned guard across any sequence that calls
/// [`with_stdout_redirected`] or [`with_stdin_redirected`].  The lock is
/// non-reentrant: nested redirect calls under a single guard are fine, but
/// nested calls to `lock_stdio_redirect` will deadlock.
#[cfg(any(feature = "coreutils", feature = "diffutils"))]
pub(crate) fn lock_stdio_redirect() -> std::sync::MutexGuard<'static, ()> {
    STDIO_REDIRECT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ── Windows in-process stdout capture ────────────────────────────────────
//
// Redirects the Win32 stdout handle to a pipe, calls f(), then restores.
// Rust's Windows stdio queries GetStdHandle on each write, so the redirect
// is transparent to println! / uutils internals.
//
// Caller must hold `lock_stdio_redirect` across the call.
//
// Only compiled when a feature that uses uutils shims is enabled.

/// Run `f` with stdout redirected to `writer`, then restore original stdout.
#[cfg(all(windows, any(feature = "coreutils", feature = "diffutils")))]
pub(crate) fn with_stdout_win(writer: &os_pipe::PipeWriter, f: impl FnOnce() -> i32) -> i32 {
    use std::io::Write as _;
    use std::os::windows::io::AsRawHandle;

    let _ = std::io::stdout().flush();
    let saved = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, writer.as_raw_handle() as *mut _);
    }
    let code = f();
    let _ = std::io::stdout().flush();
    unsafe {
        SetStdHandle(STD_OUTPUT_HANDLE, saved);
    }
    code
}

/// Run `f` with stdin redirected to `reader`, then restore the original stdin.
///
/// Same save/restore strategy as with_stdout_win: borrow the handle slot
/// value, swap in the pipe, swap the original back.  No duplication or
/// closing — we do not own the original handle.
#[cfg(all(windows, any(feature = "coreutils", feature = "diffutils")))]
pub(crate) fn with_stdin_win(
    reader: &crate::io::SourceReader,
    f: impl FnOnce() -> i32,
) -> i32 {
    use std::os::windows::io::AsRawHandle;

    let saved = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    unsafe {
        SetStdHandle(STD_INPUT_HANDLE, reader.as_raw_handle() as *mut _);
    }
    let code = f();
    unsafe {
        SetStdHandle(STD_INPUT_HANDLE, saved);
    }
    code
}

// ── Unix in-process stdout/stdin capture ─────────────────────────────────
//
// On Unix we redirect fd 1 (stdout) or fd 0 (stdin) to a pipe using dup/dup2,
// the same technique used in evaluator.rs apply_redirects.
//
// Caller must hold `lock_stdio_redirect` across the call.

#[cfg(all(unix, any(feature = "coreutils", feature = "diffutils")))]
pub(crate) fn with_stdout_unix(writer: &os_pipe::PipeWriter, f: impl FnOnce() -> i32) -> i32 {
    use std::io::Write as _;
    use std::os::unix::io::AsRawFd;

    let _ = std::io::stdout().flush();
    let saved = unsafe { libc::dup(libc::STDOUT_FILENO) };
    unsafe {
        libc::dup2(writer.as_raw_fd(), libc::STDOUT_FILENO);
    }
    let code = f();
    let _ = std::io::stdout().flush();
    if saved >= 0 {
        unsafe {
            libc::dup2(saved, libc::STDOUT_FILENO);
            libc::close(saved);
        }
    }
    code
}

#[cfg(all(unix, any(feature = "coreutils", feature = "diffutils")))]
pub(crate) fn with_stdin_unix(
    reader: &crate::io::SourceReader,
    f: impl FnOnce() -> i32,
) -> i32 {
    use std::os::unix::io::AsRawFd;

    let saved = unsafe { libc::dup(libc::STDIN_FILENO) };
    unsafe {
        libc::dup2(reader.as_raw_fd(), libc::STDIN_FILENO);
    }
    let code = f();
    if saved >= 0 {
        unsafe {
            libc::dup2(saved, libc::STDIN_FILENO);
            libc::close(saved);
        }
    }
    code
}

// ── Cross-platform wrappers ───────────────────────────────────────────────

/// Run `f` with stdout redirected to `writer`, then restore.
///
/// Delegates to the platform-specific implementation.
#[cfg(any(feature = "coreutils", feature = "diffutils"))]
pub(crate) fn with_stdout_redirected(writer: &os_pipe::PipeWriter, f: impl FnOnce() -> i32) -> i32 {
    #[cfg(windows)]
    {
        with_stdout_win(writer, f)
    }
    #[cfg(unix)]
    {
        with_stdout_unix(writer, f)
    }
}

#[cfg(any(feature = "coreutils", feature = "diffutils"))]
pub(crate) fn with_stdin_redirected(
    reader: &crate::io::SourceReader,
    f: impl FnOnce() -> i32,
) -> i32 {
    #[cfg(windows)]
    {
        with_stdin_win(reader, f)
    }
    #[cfg(unix)]
    {
        with_stdin_unix(reader, f)
    }
}
