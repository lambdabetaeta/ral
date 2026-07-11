//! ANSI styling: escape constants and color-gating predicates.
//!
//! Gating helpers ([`use_color`], [`use_ui_color`]) consult a cached
//! [`TerminalState`] seeded once at REPL startup via [`set_terminal`].
//! When the cache is empty, [`use_color`] falls back to inline probing
//! (batch runs and early-startup errors), whereas [`use_ui_color`] is
//! cache-only and yields false until [`set_terminal`] has run.
//!
//! Value-output styling (the REPL's `=> ` prefix and color) lives in the
//! `ral` crate's `repl::theme` module — it is configured from the rc file
//! and consumed only by the REPL's value-printing path.

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

// ── OSC sequence builders ────────────────────────────────────────────────
//
// Each builder returns an owned `String` so callers can write the sequence
// to any sink (stderr, an external printer, a captured byte buffer for
// tests) without committing to an IO target here.  Gating — *should we
// emit?* — lives in `TerminalState::ui_*_ok`; this module only knows how
// to format a well-formed sequence once a caller has decided to.
//
// Two string terminators appear in the wild: BEL (`\x07`) and ST
// (`ESC \`, `\x1b\\`).  Each builder's own doc records which it emits
// and why.

/// Build an OSC 0 sequence to set the terminal window/icon title.
///
/// `ESC ] 0 ; <title> BEL` — OSC introducer, parameter 0 (sets both
/// icon and window title), content, BEL terminator.  BEL is the
/// broadly-portable alternative to the ST string terminator
/// (`ESC \`); xterm, iTerm2, GNOME Terminal, Windows Terminal, and
/// every modern multiplexer all accept it.
pub fn osc_set_title(title: &str) -> String {
    format!("\x1b]0;{title}\x07")
}

/// Build an OSC 8 hyperlink that wraps `text` with a link to `uri`.
///
/// `ESC ] 8 ; ; <uri> ESC \ <text> ESC ] 8 ; ; ESC \` — open with the
/// URI, write the visible text, close with an empty-URI OSC 8.  The
/// empty parameter slot is reserved for `id=…` and similar attributes
/// we do not currently produce.
///
/// Embedded ESC bytes in either argument would terminate the sequence
/// prematurely; callers that may receive untrusted input should
/// sanitise first.  Newlines are passed through as-is.
pub fn osc8_link(uri: &str, text: &str) -> String {
    format!("\x1b]8;;{uri}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// Build an OSC 52 sequence that asks the terminal to write `payload`
/// to the system clipboard.
///
/// `ESC ] 52 ; c ; <base64> BEL` — `c` selects the system clipboard;
/// the payload must already be base64-encoded by the caller.  Pushing
/// the encoder up to the call site keeps `core` dependency-free
/// (`ral` and `exarch` both have the `base64` crate available).
///
/// BEL, not ST: tmux's own OSC parser accepts either terminator (its
/// `input.c` notes OSC "may be terminated by \007 as well as ST"), but
/// tmux's `Ms` capability — what it uses to relay a copy to the outer
/// terminal — itself emits BEL, and some terminals that implement OSC
/// 52 do not recognise the ST form at all.  BEL is therefore the
/// terminator most likely to be understood end to end; this is the one
/// builder every yank call site (REPL and exarch alike) should use.
///
/// Reads (`ESC ] 52 ; c ; ? ST`) are intentionally not provided: the
/// permission-prompt landscape across terminals is too uneven, and
/// `TerminalState` only surfaces the write capability.
pub fn osc52_copy(base64_payload: &str) -> String {
    format!("\x1b]52;c;{base64_payload}\x07")
}

// ── Escape-sequence scanning ───────────────────────────────────────────────

const ESC: u8 = 0x1b;

/// Byte length of the escape sequence beginning at `bytes[at]`, which must
/// be `ESC` (`0x1b`).
///
/// Recognises the introducers that carry no visible payload:
///   * CSI (`ESC [` … final byte `0x40..=0x7e`);
///   * the string sequences OSC/DCS/SOS/PM/APC (`ESC` `] P X ^ _` …),
///     terminated by BEL (`0x07`) or ST (`ESC \`);
///   * any other two-byte escape (`ESC` + one final byte).
///
/// A non-ASCII byte after `ESC` is visible payload, not an escape final,
/// so the lone `ESC` is consumed (length 1) to avoid landing mid-codepoint.
/// An `ESC` at the very end of the slice is also length 1.
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
/// Escape spans are recognised by [`escape_seq_len`]; the bytes between
/// them are copied verbatim, so the result is the prompt/line as it would
/// appear with styling removed (no cursor-motion simulation — see
/// exarch's `digest::visible_text` for that).
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

/// Seed the cached `TerminalState` consulted by `use_color` and `use_ui_color`.
/// Call once per process after probing.  Subsequent calls are silently ignored.
pub fn set_terminal(t: &TerminalState) {
    let _ = CACHED_TERMINAL.set(*t);
}

/// Whether to emit ANSI color on stderr (diagnostics, errors, warnings).
///
/// Consults the cached `TerminalState` when available so all ANSI gating
/// agrees on one source of truth.  Falls back to inline probing for batch
/// runs and early-startup errors.
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

/// Whether to emit ANSI color on stdout (REPL value output, help, etc.).
///
/// Checks `ui_ansi_ok()` — stdout tty + TERM + `NO_COLOR` — rather than the
/// stderr-oriented `stderr_ansi_ok()` used by `use_color`.
pub fn use_ui_color() -> bool {
    CACHED_TERMINAL.get().is_some_and(TerminalState::ui_ansi_ok)
}

/// Return `code` when `enabled` is true, otherwise the empty string.
///
/// Convenience for the common `if color { "\x1b[...]" } else { "" }` pattern.
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
        // An empty URI is the close-link sentinel; callers shouldn't use
        // it to wrap text, but the builder must still produce a parseable
        // sequence rather than a malformed prefix.
        assert_eq!(osc8_link("", "text"), "\x1b]8;;\x1b\\text\x1b]8;;\x1b\\");
    }

    #[test]
    fn osc52_copy_uses_system_clipboard_target_and_bel() {
        // Payload is opaque to this builder — `aGVsbG8=` is "hello" in
        // base64 but we don't decode it here, we just splice.
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
        // OSC (BEL- and ST-terminated), and the DCS/SOS/PM/APC introducers
        // the prompt stripper covers — all payload-free.
        assert_eq!(strip("pre\x1b]0;title\x07post"), "prepost");
        assert_eq!(strip("a\x1b]8;;https://x\x1b\\b"), "ab");
        assert_eq!(strip("a\x1bPq…data…\x1b\\b"), "ab");
        assert_eq!(strip("a\x1b_payload\x07b"), "ab");
    }

    #[test]
    fn strip_keeps_multibyte_char_after_bare_esc() {
        // A non-ASCII byte after ESC is payload; consume the lone ESC so
        // the char survives intact.
        assert_eq!(strip("\x1bλ tail"), "λ tail");
    }

    #[test]
    fn escape_seq_len_measures_each_introducer() {
        assert_eq!(escape_seq_len(b"\x1b[0m", 0), 4);
        assert_eq!(escape_seq_len(b"\x1b]0;t\x07", 0), 6);
        assert_eq!(escape_seq_len(b"\x1bX", 0), 2);
        // ESC at end of slice, and ESC + non-ASCII, both consume ESC alone.
        assert_eq!(escape_seq_len(b"\x1b", 0), 1);
        assert_eq!(escape_seq_len("\x1bλ".as_bytes(), 0), 1);
    }
}
