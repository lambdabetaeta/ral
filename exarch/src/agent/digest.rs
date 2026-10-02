//! Tool-result rendering for the model's history and the transcript.
//!
//! Each named section of a [`shell_eval::ToolResult`] is clipped at its own
//! cap, so one oversized stream cannot crowd out the others.  The string the
//! model reads on later turns is the one the transcript records, so the user
//! never sees more of a result than the model did.  These caps bound a single
//! result; the whole history is bounded by eviction
//! ([`gauge`](crate::agent::gauge)).

use crate::shell_eval;
use std::fmt::Write;

/// Head+tail caps, one per tool-result section.  Values get the most room:
/// a structured return often carries the agent's working set.
const VALUE_CAP: usize = 20_000;
const STDOUT_CAP: usize = 10_000;
const STDERR_CAP: usize = 10_000;

/// Cap for a `Report::Static` blob (parse / type errors), which
/// the model reads whole and cannot query — so it sits well under the
/// section caps: a diagnostic past a few KB is noise.
pub const OPAQUE_CAP: usize = 3000;

/// The elided bytes are kept nowhere, so re-running the command reproduces
/// the same cut; the model's only recourse is to ask for less.
const ELISION_NUDGE: &str = "; narrow the output by using within/filter/take/view-text/tail";

/// Cap `text` at `cap` bytes, measured on what a terminal would show of it
/// ([`ral_core::ansi::visible`]) rather than the raw bytes, eliding the
/// middle when it does not fit.
pub fn clip(text: &str, cap: usize) -> String {
    let plain = ral_core::ansi::visible(text);
    head_tail(&plain, cap, ELISION_NUDGE).unwrap_or(plain)
}

/// Render `r` as the block the model receives on later turns.
///
/// `STDOUT:` / `STDERR:` / `VALUE:` / `EXIT:`, each body clipped at its own
/// cap. The `TURN:` stamp that follows is `agent::shell`'s to append: a fact
/// of the log, not of the result.
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

/// Head+tail digest with an `[elided N bytes{extra}]` marker, or `None` if
/// `s` already fits in `cap`.
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

/// Back from `idx` to a newline within a small window, else the nearest
/// UTF-8 boundary at or before it.  The newline itself is excluded: the
/// elision banner supplies that break, and two would show as a blank line.
fn align_cut_back(s: &str, idx: usize) -> usize {
    const WINDOW: usize = 1024;
    let lo = idx.saturating_sub(WINDOW);
    if let Some(off) = s.as_bytes()[lo..idx].iter().rposition(|&b| b == b'\n') {
        return lo + off;
    }
    ral_core::text::floor_char_boundary(s, idx)
}

/// Forward from `idx` to one past a newline within a small window, else the
/// nearest UTF-8 boundary at or after it.
fn align_cut_forward(s: &str, idx: usize) -> usize {
    const WINDOW: usize = 1024;
    let hi = (idx + WINDOW).min(s.len());
    if let Some(off) = s.as_bytes()[idx..hi].iter().position(|&b| b == b'\n') {
        return idx + off + 1;
    }
    ral_core::text::ceil_char_boundary(s, idx)
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
        // The head drops its trailing newline, so the banner is not preceded
        // by a blank line.
        assert!(!out.contains("\n\n... [elided"));
        assert!(!out.contains(&"X".repeat(1000)));
        assert!(out.len() <= CAP + 64);
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
        let stdout = "noise\n".repeat(20_000);
        let stderr = "ERROR: division by zero at line 42\n".to_string();
        let out = render(&tr(&stdout, &stderr, None, 1));
        assert!(out.contains(&format!("STDERR:\n{stderr}")));
        assert!(out.contains("elided"));
    }

    #[test]
    fn render_value_gets_more_room_than_stdout() {
        // 12_000 sits between the two caps.
        let body = "x".repeat(12_000);
        assert!(!render(&tr("", "", Some(&body), 0)).contains("elided"));
        assert!(render(&tr(&body, "", None, 0)).contains("elided"));
    }

    #[test]
    fn clip_passes_short_input_through() {
        assert_eq!(clip("hi", 1024), "hi");
    }

    #[test]
    fn clip_elides_oversize_inline_with_nudge() {
        let body = "y".repeat(STDOUT_CAP * 2);
        let out = clip(&body, STDOUT_CAP);
        // Nothing spills to a file the model could read the rest from.
        assert!(out.contains("elided"));
        assert!(out.contains("narrow the output"));
        assert!(!out.contains("full at "));
        assert!(out.len() <= STDOUT_CAP + ELISION_NUDGE.len() + 64);
    }

    #[test]
    fn clip_measures_the_visible_text() {
        let raw = "\x1b[31m".repeat(2000) + "tiny";
        assert!(raw.len() > 1024);
        assert_eq!(clip(&raw, 1024), "tiny");
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
