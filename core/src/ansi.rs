//! ANSI escape constants, OSC sequence builders, and the color gate.
//!
//! [`use_color`] (stderr) and [`use_ui_color`] (stdout) consult a
//! [`TerminalState`] that each frontend seeds once at startup through
//! [`set_terminal`], re-exported as `diagnostic::set_terminal`.  Until then
//! [`use_color`] probes inline, so early-startup errors still color.  The OSC
//! builders only format; whether a sequence may be emitted is decided by the
//! `TerminalState::ui_*_ok` predicates.

use std::sync::OnceLock;

use crate::io::TerminalState;

// ── Constants ─────────────────────────────────────────────────────────────

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const DIM: &str = "\x1b[2m";
pub const UNDERLINE: &str = "\x1b[4m";
pub const REVERSE: &str = "\x1b[7m";

pub const BLACK: &str = "\x1b[0;30m";
pub const RED: &str = "\x1b[0;31m";
pub const GREEN: &str = "\x1b[0;32m";
pub const YELLOW: &str = "\x1b[0;33m";
pub const BLUE: &str = "\x1b[0;34m";
pub const MAGENTA: &str = "\x1b[0;35m";
pub const CYAN: &str = "\x1b[0;36m";
pub const WHITE: &str = "\x1b[0;37m";

pub const BOLD_RED: &str = "\x1b[1;31m";
pub const BOLD_GREEN: &str = "\x1b[1;32m";
pub const BOLD_YELLOW: &str = "\x1b[1;33m";
pub const BOLD_BLUE: &str = "\x1b[1;34m";
pub const BOLD_CYAN: &str = "\x1b[1;36m";

pub const UNDERLINE_RED: &str = "\x1b[4;31m";

/// SGR escape for one of the eight standard color names, case-insensitively.
///
/// Every other name returns `None`, and ral's `repl::theme` reads `None` as
/// "no color" — a typo in the RC file drops value styling instead of erroring.
pub fn named_color(name: &str) -> Option<String> {
    match name.to_ascii_lowercase().as_str() {
        "black" => Some(BLACK.into()),
        "red" => Some(RED.into()),
        "green" => Some(GREEN.into()),
        "yellow" => Some(YELLOW.into()),
        "blue" => Some(BLUE.into()),
        "magenta" => Some(MAGENTA.into()),
        "cyan" => Some(CYAN.into()),
        "white" => Some(WHITE.into()),
        _ => None,
    }
}

// ── OSC sequence builders ────────────────────────────────────────────────

/// OSC 0 — set the terminal's window and icon title.
///
/// Terminated with BEL rather than ST, which every terminal and multiplexer
/// in practice accepts.
pub fn osc_set_title(title: &str) -> String {
    format!("\x1b]0;{title}\x07")
}

/// OSC 8 — wrap `text` in a hyperlink to `uri`, closed by an empty-URI OSC 8.
///
/// An ESC byte in either argument ends the sequence early, so sanitise
/// anything untrusted before calling.
pub fn osc8_link(uri: &str, text: &str) -> String {
    format!("\x1b]8;;{uri}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// OSC 52 — ask the terminal to put `base64_payload` on the system clipboard.
///
/// The caller encodes, which is what keeps `core` free of a base64 dependency.
/// Terminated with BEL, not ST: tmux relays a copy through its `Ms` capability,
/// which emits BEL, and some terminals implementing OSC 52 never learned ST.
/// Every yank path, REPL and exarch alike, goes through here.
///
/// Write only.  Clipboard reads are not offered because the permission prompts
/// vary too much between terminals, and `TerminalState` gates only the write.
pub fn osc52_copy(base64_payload: &str) -> String {
    format!("\x1b]52;c;{base64_payload}\x07")
}

// ── Escape-sequence scanning ───────────────────────────────────────────────

const ESC: u8 = 0x1b;

/// Byte length of the escape sequence at `bytes[at]`, which must be `ESC`.
///
/// Covers CSI, the string sequences OSC/DCS/SOS/PM/APC (BEL- or ST-terminated),
/// and plain two-byte escapes — every introducer carrying no visible payload.
/// A non-ASCII byte after `ESC` is payload, not a final, so the lone `ESC` is
/// consumed to avoid landing mid-codepoint; a trailing `ESC` measures 1 too.
pub fn escape_seq_len(bytes: &[u8], at: usize) -> usize {
    debug_assert_eq!(bytes[at], ESC);
    match bytes.get(at + 1) {
        Some(b'[') => {
            let mut i = at + 2;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            i += usize::from(i < bytes.len());
            i - at
        }
        Some(b']' | b'P' | b'X' | b'^' | b'_') => {
            let mut i = at + 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    i += 1;
                    break;
                }
                if bytes[i] == ESC && bytes.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    break;
                }
                i += 1;
            }
            i - at
        }
        Some(b) if b.is_ascii() => 2,
        _ => 1,
    }
}

/// Drop every ANSI escape sequence from `s`, leaving the visible text.
///
/// Styling only: carriage returns and backspaces survive untouched, since
/// nothing here replays cursor motion.  exarch's `digest::visible_text` does.
pub fn strip(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == ESC {
            i += escape_seq_len(bytes, i);
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != ESC {
            i += 1;
        }
        out.push_str(&s[start..i]);
    }
    out
}

// ── Color-gating ──────────────────────────────────────────────────────────

static CACHED_TERMINAL: OnceLock<TerminalState> = OnceLock::new();

/// Seed the cached [`TerminalState`] once per process, after probing.  The
/// first call wins; later ones are ignored.
pub fn set_terminal(t: &TerminalState) {
    let _ = CACHED_TERMINAL.set(*t);
}

/// Whether stderr — diagnostics, errors, warnings — may carry color.
///
/// Prefers the cached snapshot so all gating agrees on one source of truth,
/// and probes inline while the cache is still empty.
pub fn use_color() -> bool {
    if let Some(t) = CACHED_TERMINAL.get() {
        return t.stderr_ansi_ok();
    }
    if anstyle_query::no_color() {
        return false;
    }
    if !anstyle_query::term_supports_ansi_color() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::io::IsTerminal;
        std::io::stderr().is_terminal()
    }
    #[cfg(windows)]
    {
        crate::io::is_console(crate::io::STD_ERROR_HANDLE)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// Whether stdout — REPL value output, help — may carry color.
///
/// Cache-only, so false until [`set_terminal`] runs, and gated on the stdout
/// predicate: stdout can be piped into a pager while stderr stays a tty.
pub fn use_ui_color() -> bool {
    CACHED_TERMINAL.get().is_some_and(TerminalState::ui_ansi_ok)
}

/// `code` when `enabled`, the empty string otherwise.
pub fn when(enabled: bool, code: &'static str) -> &'static str {
    if enabled { code } else { "" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_set_title_uses_bel_terminator() {
        assert_eq!(osc_set_title("ral"), "\x1b]0;ral\x07");
        assert_eq!(osc_set_title(""), "\x1b]0;\x07");
    }

    #[test]
    fn osc8_link_wraps_visible_text_with_st() {
        let s = osc8_link("https://example.com", "click me");
        assert_eq!(s, "\x1b]8;;https://example.com\x1b\\click me\x1b]8;;\x1b\\",);
    }

    #[test]
    fn osc8_link_empty_uri_still_well_formed() {
        // The empty URI is the close-link sentinel; wrapping text in it is a
        // caller error, but must still parse rather than truncate the stream.
        assert_eq!(osc8_link("", "text"), "\x1b]8;;\x1b\\text\x1b]8;;\x1b\\");
    }

    #[test]
    fn osc52_copy_uses_system_clipboard_target_and_bel() {
        // The payload is opaque to the builder; `aGVsbG8=` is "hello", spliced.
        assert_eq!(osc52_copy("aGVsbG8="), "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn strip_drops_sgr_and_other_csi() {
        assert_eq!(strip("\x1b[31mred\x1b[0m $ "), "red $ ");
        assert_eq!(strip("ab\x1b[2Kcd"), "abcd");
        assert_eq!(strip("\x1b[1Gprompt"), "prompt");
    }

    #[test]
    fn strip_drops_string_sequences() {
        assert_eq!(strip("pre\x1b]0;title\x07post"), "prepost");
        assert_eq!(strip("a\x1b]8;;https://x\x1b\\b"), "ab");
        assert_eq!(strip("a\x1bPq…data…\x1b\\b"), "ab");
        assert_eq!(strip("a\x1b_payload\x07b"), "ab");
    }

    #[test]
    fn strip_keeps_multibyte_char_after_bare_esc() {
        assert_eq!(strip("\x1bλ tail"), "λ tail");
    }

    #[test]
    fn escape_seq_len_measures_each_introducer() {
        assert_eq!(escape_seq_len(b"\x1b[0m", 0), 4);
        assert_eq!(escape_seq_len(b"\x1b]0;t\x07", 0), 6);
        assert_eq!(escape_seq_len(b"\x1bX", 0), 2);
        assert_eq!(escape_seq_len(b"\x1b", 0), 1);
        assert_eq!(escape_seq_len("\x1bλ".as_bytes(), 0), 1);
    }
}
