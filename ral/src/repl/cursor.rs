/// Query the cursor column via ANSI CPR (ESC[6n → ESC[row;colR).
/// Temporarily switches stdin to raw mode to read the response without
/// waiting for a newline. Returns `None` on any error or timeout.
pub(super) fn query_cursor_col() -> Option<usize> {
    use libc::{TCSANOW, tcgetattr, tcsetattr};
    use std::io::Write;

    unsafe {
        let mut orig = std::mem::zeroed::<libc::termios>();
        if tcgetattr(0, &raw mut orig) != 0 {
            return None;
        }
        let mut raw = orig;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1; // 100 ms timeout per read(2)
        if tcsetattr(0, TCSANOW, &raw const raw) != 0 {
            return None;
        }

        let _ = std::io::stdout().write_all(b"\x1b[6n");
        let _ = std::io::stdout().flush();

        let mut buf = [0u8; 32];
        let mut len = 0usize;
        loop {
            if len >= buf.len() {
                break;
            }
            let n = libc::read(0, buf.as_mut_ptr().add(len).cast::<libc::c_void>(), 1);
            if n <= 0 {
                break;
            }
            len += 1;
            if buf[len - 1] == b'R' {
                break;
            }
        }

        tcsetattr(0, TCSANOW, &raw const orig);

        if len < 6 || buf[0] != b'\x1b' || buf[1] != b'[' {
            return None;
        }
        let inner = &buf[2..len - 1]; // ESC[ … R
        let semi = inner.iter().position(|&b| b == b';')?;
        std::str::from_utf8(&inner[semi + 1..]).ok()?.parse().ok()
    }
}

/// If the cursor is not at column 1, print a reverse-video `%` marker and
/// move to a fresh line (zsh PROMPT_SP style), preserving partial output.
pub(super) fn partial_line_marker() {
    use std::io::Write;
    if query_cursor_col().is_some_and(|col| col > 1) {
        let marker = format!("{}%{}\n", ral_core::ansi::REVERSE, ral_core::ansi::RESET);
        let _ = std::io::stdout().write_all(marker.as_bytes());
        let _ = std::io::stdout().flush();
    }
}
