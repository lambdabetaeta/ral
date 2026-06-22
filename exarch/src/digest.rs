//! Tool-result rendering for the model's conversation history and the
//! post-mortem transcript.
//!
//! A single view over a [`shell_eval::ToolResult`]: each named section
//! (`STDOUT:` / `STDERR:` / `VALUE:` / `EXIT:`) flows through
//! [`clip`] at its own per-section cap.  A section over its cap keeps a
//! head+tail digest and elides the middle, with a banner nudging the
//! model to scope the query at its source and read the result in slices
//! rather than dump it whole.  The same rendering is what the model
//! receives on later turns and what the transcript records — the user
//! never sees more of a result than the model does.
//!
//! Non-`ral` tools call [`clip`] directly on their own output (fff,
//! agent replies, opaque parse-error blobs) with their own caps.

use crate::shell_eval;
use std::fmt::Write;

/// Head+tail caps, one per tool-result section.  [`clip`] keeps ~half
/// the cap as head and ~half as tail, so a section over its cap is
/// elided in the middle rather than truncated at the end.  Sized by how
/// cheap the elided part is to recover: a value is bound and re-sliced
/// for free (`take`/index), so it stays tight; stdout is ephemeral and
/// only recoverable by re-running the command, so it gets room to spare
/// the re-run; stderr is diagnostic and wants to survive whole.
const VALUE_CAP: usize = 1500;
const STDOUT_CAP: usize = 4000;
const STDERR_CAP: usize = 3000;

/// Cap for one `fff` tool result.
pub const FFF_CAP: usize = 2000;

/// Cap for an `Outcome::Static` blob (parse / type errors etc.) — a
/// diagnostic the model reads in full and cannot query, so it gets the
/// same room as stderr.
pub const OPAQUE_CAP: usize = 3000;

/// Cap for the final assistant text returned by a child agent — a
/// curated, non-bindable report, so it keeps the most room.
pub const AGENT_REPLY_CAP: usize = 6000;

/// History size in bytes at which [`crate::session::Session::compact`]
/// kicks in.  500 KB keeps roughly a dozen tool results in flight
/// before compaction.
pub const COMPACT_THRESHOLD: usize = 500 * 1024;

/// Appended to an elision banner.  The elided bytes are gone, so the
/// model's recourse is to narrow what it asked for — scope the query at
/// its source, or read the result in slices.  Re-running the command,
/// or `$x`-dumping the whole binding, only reproduces the same cut.
const ELISION_NUDGE: &str = "; complete but clamped — narrow it (within/filter to scope, take/view-text/tail to slice); re-running repeats the cut";

/// Cap `text` at `cap` bytes for the model's history and the transcript.
///
/// If the visible text ([`visible_text`]) fits within `cap`, it passes
/// through unchanged.  Otherwise the middle is elided to a head+tail
/// digest whose banner ([`ELISION_NUDGE`]) points the model at
/// narrowing what it asked for; the elided bytes are not retained
/// anywhere.
///
/// Called by [`render`] per section and by each non-`ral` tool on its
/// own output.  One pass over the text either way.
pub fn clip(text: &str, cap: usize) -> String {
    let plain = visible_text(text);
    head_tail(&plain, cap, ELISION_NUDGE).unwrap_or(plain)
}

/// Render `r` as the named-section block the model receives on later
/// turns — `STDOUT:` / `STDERR:` / `VALUE:` / `EXIT:`, each
/// body clipped at its own cap.  This is also the string the transcript
/// records, so the user never sees more of a result than the model.
pub fn render(r: &shell_eval::ToolResult) -> String {
    let mut out = String::new();
    if !r.stdout.is_empty() {
        let s = String::from_utf8_lossy(&r.stdout);
        out.push_str("STDOUT:\n");
        out.push_str(&clip(&s, STDOUT_CAP));
    }
    if !r.stderr.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        let s = String::from_utf8_lossy(&r.stderr);
        out.push_str("STDERR:\n");
        out.push_str(&clip(&s, STDERR_CAP));
    }
    if let Some(v) = &r.value {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("VALUE:\n");
        out.push_str(&clip(v, VALUE_CAP));
        out.push('\n');
    }
    let _ = write!(out, "\nEXIT: {}", r.exit);
    out
}

/// Head+tail digest.  Returns `None` if `s` fits in `cap`.  Otherwise
/// returns a digest with an `[elided N bytes{extra}]` marker.  Cuts
/// prefer a newline boundary in a small window, else a UTF-8 boundary.
fn head_tail(s: &str, cap: usize, extra: &str) -> Option<String> {
    if s.len() <= cap {
        return None;
    }
    let half = cap.saturating_sub(64 + extra.len()) / 2;
    let head_end = align_cut_back(s, half);
    let tail_start = align_cut_forward(s, s.len() - half);
    let omitted = tail_start - head_end;
    Some(format!(
        "{}\n... [elided {omitted} bytes{extra}] ...\n{}",
        &s[..head_end],
        &s[tail_start..],
    ))
}

/// Walk back from `idx` to a newline within a small window, falling
/// back to the nearest UTF-8 boundary at or before `idx`.  The newline
/// itself is excluded — the elision banner supplies the line break — so
/// the head doesn't end in a doubled newline.
fn align_cut_back(s: &str, idx: usize) -> usize {
    const WINDOW: usize = 1024;
    let lo = idx.saturating_sub(WINDOW);
    if let Some(off) = s.as_bytes()[lo..idx].iter().rposition(|&b| b == b'\n') {
        return lo + off;
    }
    ral_core::text::floor_char_boundary(s, idx)
}

/// Walk forward from `idx` to one past a newline within a small
/// window, falling back to the nearest UTF-8 boundary at or after
/// `idx`.
fn align_cut_forward(s: &str, idx: usize) -> usize {
    const WINDOW: usize = 1024;
    let hi = (idx + WINDOW).min(s.len());
    if let Some(off) = s.as_bytes()[idx..hi].iter().position(|&b| b == b'\n') {
        return idx + off + 1;
    }
    ral_core::text::ceil_char_boundary(s, idx)
}

/// Reduce `text` to the visible payload a terminal would leave on
/// screen.  CSI and OSC escape sequences are dropped (Ariadne styling,
/// forced colour).  Within a line, a carriage return rewinds the write
/// position to column zero so later text overwrites earlier text — a
/// progress meter collapses to its final frame — and a backspace
/// rewinds one cell, so nroff-style overstrike keeps the last glyph
/// written.  A cell is one `char`, not a width-aware grapheme cluster;
/// that is exact for the meter and overstrike output this simulates
/// and a harmless approximation beyond it.
fn visible_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut line: Vec<char> = Vec::new();
    let mut col = 0usize;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => i += ral_core::ansi::escape_seq_len(bytes, i),
            b'\n' => {
                out.extend(line.drain(..));
                out.push('\n');
                col = 0;
                i += 1;
            }
            b'\r' => {
                col = 0;
                i += 1;
            }
            0x08 => {
                col = col.saturating_sub(1);
                i += 1;
            }
            _ => {
                let ch = text[i..]
                    .chars()
                    .next()
                    .expect("slice starts at char boundary");
                if col < line.len() {
                    line[col] = ch;
                } else {
                    line.push(ch);
                }
                col += 1;
                i += ch.len_utf8();
            }
        }
    }
    out.extend(line.drain(..));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr(stdout: &str, stderr: &str, value: Option<&str>, exit: i32) -> shell_eval::ToolResult {
        shell_eval::ToolResult {
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            value: value.map(str::to_string),
            exit,
        }
    }

    #[test]
    fn head_tail_keeps_both_ends_aligned_to_newlines() {
        const CAP: usize = 16 * 1024;
        let head = "FIRST_LINE\n".repeat(2000);
        let tail = "LAST_LINE\n".repeat(2000);
        let input = format!("{head}{}{tail}", "X".repeat(50_000));
        let out = head_tail(&input, CAP, "").unwrap();
        assert!(out.contains("FIRST_LINE") && out.contains("LAST_LINE"));
        assert!(out.contains("\n... [elided") && out.contains("] ...\n"));
        // The newline-aligned head excludes its trailing newline; the
        // banner supplies the break, so no blank line precedes it.
        assert!(!out.contains("\n\n... [elided"));
        assert!(!out.contains(&"X".repeat(1000)));
        assert!(out.len() <= CAP + 64);
    }

    #[test]
    fn head_tail_passes_short_input_through() {
        assert!(head_tail("short", 1024, "").is_none());
    }

    #[test]
    fn handles_utf8_at_cut_boundary() {
        let input = "λ".repeat(20_000);
        assert!(head_tail(&input, 16 * 1024, "").unwrap().contains("elided"));
    }

    #[test]
    fn render_keeps_canonical_section_order() {
        let r = tr("abc\n", "err\n", Some("v"), 0);
        assert_eq!(
            render(&r),
            "STDOUT:\nabc\n\nSTDERR:\nerr\n\nVALUE:\nv\n\nEXIT: 0",
        );
    }

    #[test]
    fn render_strips_ansi_from_streams() {
        let r = tr("\x1b[31mred\x1b[0m\n", "\x1b[1;33mwarn\x1b[0m\n", None, 1);
        let out = render(&r);
        assert!(out.contains("STDOUT:\nred\n"));
        assert!(out.contains("STDERR:\nwarn\n"));
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn render_caps_each_section_independently() {
        // A huge stdout is elided; the small stderr passes through whole
        // — each section is held to its own cap, not a shared budget.
        let stdout = "noise\n".repeat(20_000);
        let stderr = "ERROR: division by zero at line 42\n".to_string();
        let out = render(&tr(&stdout, &stderr, None, 1));
        assert!(out.contains(&format!("STDERR:\n{stderr}")));
        assert!(out.contains("elided"));
    }

    #[test]
    fn render_value_clamps_tighter_than_stdout() {
        // A body between the two caps is clamped as a VALUE (1500) but
        // passes through as STDOUT (4000) — the caps differ by section.
        let body = "x".repeat(1800);
        assert!(render(&tr("", "", Some(&body), 0)).contains("elided"));
        assert!(!render(&tr(&body, "", None, 0)).contains("elided"));
    }

    #[test]
    fn clip_passes_short_input_through() {
        assert_eq!(clip("hi", 1024), "hi");
    }

    #[test]
    fn clip_elides_oversize_inline_with_nudge() {
        let body = "y".repeat(STDOUT_CAP * 2);
        let out = clip(&body, STDOUT_CAP);
        // Elided in place with the narrow-it nudge; nothing is written
        // to disk to recover later.
        assert!(out.contains("elided"));
        assert!(out.contains("clamped"));
        assert!(!out.contains("full at "));
        assert!(out.len() <= STDOUT_CAP + ELISION_NUDGE.len() + 64);
    }

    #[test]
    fn clip_strips_ansi_before_measuring() {
        // Pure ANSI that strips to a tiny payload passes through.
        let raw = "\x1b[31m".repeat(2000) + "tiny";
        assert!(raw.len() > 1024);
        assert_eq!(clip(&raw, 1024), "tiny");
    }

    #[test]
    fn visible_text_keeps_multibyte_char_after_bare_esc() {
        assert_eq!(visible_text("\x1bλ tail"), "λ tail");
    }

    #[test]
    fn carriage_return_collapses_to_final_frame() {
        assert_eq!(visible_text("0%\r50%\r100%\n"), "100%\n");
    }

    #[test]
    fn crlf_is_a_plain_line_ending() {
        assert_eq!(visible_text("alpha\r\nbeta\r\n"), "alpha\nbeta\n");
    }

    #[test]
    fn overwrite_shorter_than_frame_keeps_the_tail() {
        // A terminal leaves the unoverwritten cells standing; so do we.
        assert_eq!(visible_text("loading\rdone"), "doneing");
    }

    #[test]
    fn backspace_overstrike_keeps_last_glyph() {
        // nroff bold (b BS b) and underline (_ BS x) both resolve to
        // the final glyph; a backspace at column zero is inert.
        assert_eq!(visible_text("b\u{8}bo\u{8}o000"), "bo000");
        assert_eq!(visible_text("_\u{8}x"), "x");
        assert_eq!(visible_text("\u{8}a"), "a");
    }

    #[test]
    fn clip_measures_after_overwrite_simulation() {
        // Megabytes of progress-meter churn collapse to one final frame
        // and pass through whole — the cap is measured on what a
        // terminal would actually show.
        let churn = "spinner\r".repeat(100_000) + "done.  ";
        assert!(churn.len() > STDOUT_CAP);
        assert_eq!(clip(&churn, STDOUT_CAP), "done.  ");
    }

    #[test]
    fn clip_strips_ansi_from_oversize_input() {
        let body = format!("\x1b[31m{}\x1b[0m", "y".repeat(STDOUT_CAP * 2));
        let out = clip(&body, STDOUT_CAP);
        assert!(!out.contains('\x1b'));
        assert!(out.contains("elided"));
    }
}
