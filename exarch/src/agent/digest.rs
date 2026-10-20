//! Tool-result rendering for the model's history and the transcript.
//!
//! Each named section of a [`shell_eval::ToolResult`] is clipped at its own
//! cap, so one oversized stream cannot crowd out the others.  The string the
//! model reads on later turns is the one the transcript records, so the user
//! never sees more of a result than the model did.  These caps bound a single
//! result; the whole history is bounded by eviction
//! ([`gauge`](crate::agent::gauge)).

use crate::shell_eval::{self, clip::clip};
use std::fmt::Write;

/// Head+tail caps, one per tool-result section.  Values get the most room:
/// a structured return often carries the agent's working set.
const VALUE_CAP: usize = 20_000;
const STDOUT_CAP: usize = 10_000;
const STDERR_CAP: usize = 10_000;

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
