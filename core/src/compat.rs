//! Platform compatibility shims.
//!
//! TTY detection and ANSI virtual-terminal-processing setup on Windows.
//! In-process stdin/stdout redirection for embedded shims is gone:
//! coreutils dispatches through a helper subprocess (see
//! `core/src/builtins/uutils.rs`) and `diffutils` is no longer bundled,
//! so the kernel handles fd 0/1 contention across process boundaries.

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

// STD_*_HANDLE constants mirror the Win32 values passed to GetStdHandle.
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

