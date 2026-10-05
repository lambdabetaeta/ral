//! The zsh-style partial-line marker: before each prompt, a reverse-video `%`
//! closes output that ended mid-line, and the prompt starts at column 1 of a
//! clean line.  Neither platform reads stdin, so type-ahead reaches the editor.

use ral_core::ansi::{RESET, REVERSE};
use std::io::Write;

/// zsh's `PROMPT_SP`.  From column 1, `%` and `width - 1` spaces end in the
/// pending-wrap state on the same line, which `\r\x1b[K` erases; from
/// mid-line they wrap, so only the spill is erased and the `%` stays.
#[cfg(unix)]
pub(super) fn partial_line_marker() {
    let Some(pad) = rustix::termios::tcgetwinsize(rustix::stdio::stdout())
        .ok()
        .and_then(|size| usize::from(size.ws_col).checked_sub(1))
    else {
        return;
    };
    let _ = write!(std::io::stdout(), "{REVERSE}%{RESET}{:pad$}\r\x1b[K", "");
    let _ = std::io::stdout().flush();
}

/// If the cursor is not at column 1, print the marker and move to a fresh
/// line.
#[cfg(windows)]
pub(super) fn partial_line_marker() {
    if query_cursor_col().is_some_and(|col| col > 1) {
        let _ = writeln!(std::io::stdout(), "{REVERSE}%{RESET}");
    }
}

/// Query the cursor column via the Win32 console API. Returns `None`
/// if stdout is not attached to a console (e.g. piped or redirected).
/// The returned value is 1-based.
#[cfg(windows)]
fn query_cursor_col() -> Option<usize> {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
    };

    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() {
            return None;
        }
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(h, &raw mut info) == 0 {
            return None;
        }
        // A console column is never negative; `try_from` says so in the
        // type rather than by assumption, and a negative one would
        // read as "no answer" instead of an enormous column.
        usize::try_from(info.dwCursorPosition.X)
            .ok()
            .map(|col| col + 1)
    }
}
