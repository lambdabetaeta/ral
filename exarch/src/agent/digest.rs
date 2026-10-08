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
    s.floor_char_boundary(idx)
}

/// Forward from `idx` to one past a newline within a small window, else the
/// nearest UTF-8 boundary at or after it.
fn align_cut_forward(s: &str, idx: usize) -> usize {
    const WINDOW: usize = 1024;
    let hi = (idx + WINDOW).min(s.len());
    if let Some(off) = s.as_bytes()[idx..hi].iter().position(|&b| b == b'\n') {
        return idx + off + 1;
    }
    s.ceil_char_boundary(idx)
}

#[cfg(test)]
mod tests;
