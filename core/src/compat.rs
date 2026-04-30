//! Platform compatibility shims.
//!
//! TTY detection, ANSI virtual-terminal-processing setup on Windows, and
//! in-process stdin redirection for the embedded `diffutils` shims.  On
//! Unix the stdin redirection uses `dup`/`dup2`; on Windows it swaps the
//! Win32 standard-input handle slot directly.
//!
//! Coreutils used to need analogous fd 1 redirection here too, but that
//! path was removed when uutils dispatch moved to a helper subprocess —
//! the kernel handles fd 0/1 cleanly across process boundaries, which
//! the in-process design did not.  See `core/src/builtins/uutils.rs` for
//! the rationale.

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

// STD_INPUT_HANDLE is only used by `with_stdin_win` (diffutils path).
// STD_OUTPUT_HANDLE / STD_ERROR_HANDLE are used by
// `enable_virtual_terminal_processing`, always-on for Windows.
#[cfg(all(windows, feature = "diffutils"))]
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

#[cfg(all(windows, feature = "diffutils"))]
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

// ── stdio redirect serialization (diffutils only) ────────────────────────
//
// dup2 of fd 0 mutates global process state.  Sequential `cmp` / `diff`
// calls inside ral evaluate in the parent process and project an upstream
// pipe reader onto the host fd 0 via `with_stdin_redirected`, so we
// serialise them under this lock.  Coreutils used to need the same for
// fd 1 too; that path is gone now (uutils dispatch spawns a helper
// subprocess so the kernel handles fd 0/1 contention).
//
// Held by callers across the redirect/run/restore window.

#[cfg(feature = "diffutils")]
static STDIO_REDIRECT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(feature = "diffutils")]
pub(crate) fn lock_stdio_redirect() -> std::sync::MutexGuard<'static, ()> {
    STDIO_REDIRECT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(all(windows, feature = "diffutils"))]
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

#[cfg(all(unix, feature = "diffutils"))]
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

#[cfg(feature = "diffutils")]
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
