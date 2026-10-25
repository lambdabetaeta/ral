//! Terminal geometry around the prompt: the zsh-style partial-line marker —
//! before each prompt, a reverse-video `%` closes output that ended mid-line,
//! and the prompt starts at column 1 of a clean line — and the erase of an
//! abandoned edit.  Neither platform reads stdin, so type-ahead reaches the
//! editor.

use ral_core::io::ansi::{RESET, REVERSE};
use std::io::Write;
use unicode_width::UnicodeWidthStr;

/// Terminal width in columns, when stdout reports one.
#[cfg(unix)]
fn cols() -> Option<usize> {
    rustix::termios::tcgetwinsize(rustix::stdio::stdout())
        .ok()
        .map(|size| usize::from(size.ws_col))
}

/// Terminal width in columns, when stdout is a console.
#[cfg(windows)]
fn cols() -> Option<usize> {
    console_info()
        .and_then(|info| usize::try_from(info.srWindow.Right - info.srWindow.Left + 1).ok())
}

/// zsh's `PROMPT_SP`.  From column 1, `%` and `width - 1` spaces end in the
/// pending-wrap state on the same line, which `\r\x1b[K` erases; from
/// mid-line they wrap, so only the spill is erased and the `%` stays.
#[cfg(unix)]
pub(super) fn partial_line_marker() {
    let Some(pad) = cols().and_then(|cols| cols.checked_sub(1)) else {
        return;
    };
    let _ = write!(std::io::stdout(), "{REVERSE}%{RESET}{:pad$}\r\x1b[K", "");
    let _ = std::io::stdout().flush();
}

/// If the cursor is not at column 1, print the marker and move to a fresh
/// line.
#[cfg(windows)]
pub(super) fn partial_line_marker() {
    if console_info().is_some_and(|info| info.dwCursorPosition.X > 0) {
        let _ = writeln!(std::io::stdout(), "{REVERSE}%{RESET}");
    }
}

/// Erase what the editor drew for `buffer` after `prompt`, the cursor sitting
/// on the line below it: one row per terminal-width chunk of each line.
pub(super) fn erase_drawn(prompt: &str, buffer: &str) {
    let cols = cols().filter(|&c| c > 0).unwrap_or(usize::MAX);
    let rows: usize = format!("{prompt}{buffer}")
        .split('\n')
        .map(|line| line.width().div_ceil(cols).max(1))
        .sum();
    let _ = write!(std::io::stdout(), "\x1b[{rows}A\r\x1b[J");
    let _ = std::io::stdout().flush();
}

/// The console screen buffer's state via the Win32 console API, or `None` if
/// stdout is not attached to a console (e.g. piped or redirected).
#[cfg(windows)]
fn console_info() -> Option<windows_sys::Win32::System::Console::CONSOLE_SCREEN_BUFFER_INFO> {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_OUTPUT_HANDLE,
    };

    unsafe {
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if h.is_null() {
            return None;
        }
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        (GetConsoleScreenBufferInfo(h, &raw mut info) != 0).then_some(info)
    }
}
