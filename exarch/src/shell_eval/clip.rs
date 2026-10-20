//! Head+tail clipping of text the model reads: one cut, shared by a result's
//! sections and a refused or faulted call's diagnostic.

/// The elided bytes are kept nowhere, so re-running the command reproduces
/// the same cut; the model's only recourse is to ask for less.
const ELISION_NUDGE: &str = "; narrow the output by using within/filter/take/view-text/tail";

/// Cap `text` at `cap` bytes, measured on what a terminal would show of it
/// ([`ral_core::ansi::visible`]) rather than the raw bytes, eliding the
/// middle when it does not fit.
pub(crate) fn clip(text: &str, cap: usize) -> String {
    let plain = ral_core::ansi::visible(text);
    head_tail(&plain, cap, ELISION_NUDGE).unwrap_or(plain)
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
