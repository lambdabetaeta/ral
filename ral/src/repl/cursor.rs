//! Terminal geometry around the prompt: the zsh-style partial-line marker —
//! before each prompt, a reverse-video `%` closes output that ended mid-line,
//! and the prompt starts at column 1 of a clean line — and the erase of an
//! abandoned edit.  Neither platform reads stdin, so type-ahead reaches the
//! editor.

use ral_core::io::ansi::{RESET, REVERSE};
use rustyline::Config;
use std::io::Write;
use unicode_segmentation::UnicodeSegmentation as _;

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

/// Rustyline advances exact-width endings, then writes an accepted-line LF.
fn drawn_rows(prompt: &str, buffer: &str, cols: Option<usize>, config: &Config) -> usize {
    let cols = cols.filter(|&c| c > 0).unwrap_or(usize::MAX);
    let tab = usize::from(config.tab_stop()).max(1);
    let mut row = 0usize;
    let mut col = 0usize;
    for text in [prompt, buffer] {
        for grapheme in text.graphemes(true) {
            if grapheme == "\n" {
                row = row.saturating_add(1);
                col = 0;
                continue;
            }
            let width = if grapheme == "\t" {
                tab - col % tab
            } else {
                usize::from(config.grapheme_cluster_mode().width(grapheme))
            };
            col = col.saturating_add(width);
            if col > cols {
                row = row.saturating_add(1);
                col = width;
            }
        }
        // The editor measures prompt and buffer separately.
        if col == cols {
            row = row.saturating_add(1);
            col = 0;
        }
    }
    row.saturating_add(1)
}

/// Erase the accepted drawing from the row below its final cursor position.
pub(super) fn erase_drawn(prompt: &str, buffer: &str, config: &Config) {
    let rows = drawn_rows(prompt, buffer, cols(), config);
    let _ = write!(std::io::stdout(), "\x1b[{rows}A\r\x1b[J");
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustyline::{Config, GraphemeClusterMode};

    #[test]
    fn rows_follow_the_editor_cursor_and_accept_newline() {
        let config = Config::builder()
            .grapheme_cluster_mode(GraphemeClusterMode::Unicode)
            .build();
        for (prompt, buffer, rows) in [
            ("", "1234567", 1),
            ("", "12345678", 2),
            ("", "123456789", 2),
            ("123", "45678", 2),
            ("", "\t", 2),
            ("", "1234567\t", 2),
            ("", "12345678\n", 2),
            ("", "12345678\nX", 2),
            ("", "12345678\n12345678", 3),
            ("", "123456界", 2),
            ("", "1234567界", 2),
            ("", "1234567e\u{301}", 2),
            ("", "123456👩\u{200d}💻", 2),
            ("", "1234567👩\u{200d}💻", 2),
        ] {
            assert_eq!(
                drawn_rows(prompt, buffer, Some(8), &config),
                rows,
                "{prompt:?} {buffer:?}"
            );
        }
    }

    #[test]
    fn geometry_uses_the_configured_tabs_and_grapheme_mode() {
        let config = Config::builder()
            .tab_stop(4)
            .grapheme_cluster_mode(GraphemeClusterMode::Unicode)
            .build();
        assert_eq!(drawn_rows("", "\tx", Some(8), &config), 1);
        assert_eq!(drawn_rows("", "12345👩\u{200d}💻", Some(8), &config), 1);
        let config = Config::builder()
            .grapheme_cluster_mode(GraphemeClusterMode::WcWidth)
            .build();
        assert_eq!(drawn_rows("", "12345👩\u{200d}💻", Some(8), &config), 2);
    }

    #[test]
    fn unknown_or_zero_columns_fall_back_to_explicit_lines() {
        for cols in [None, Some(0), Some(usize::MAX)] {
            assert_eq!(
                drawn_rows("prompt", "界\t\nnext", cols, &Config::default()),
                2
            );
        }
    }
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
