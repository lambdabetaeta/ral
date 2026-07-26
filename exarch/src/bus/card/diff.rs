//! The diff mark's interior: a [`Hunk`] is a run of [`Row`]s, and a
//! [`Row`] is a run of [`Seg`]ments — the shape a changed line takes when
//! `similar`'s word-level diff has picked out what actually moved.
//!
//! [`whole_file_hunks`] is the one place a pair of file texts becomes this
//! structure, grouped with context the way a unified diff reads.

use serde::Serialize;

/// Total changed lines (deletions + additions) across `hunks`.
///
/// The diff magnitude, shared by [`crate::bus::card::Card::magnitude`] and
/// the renderer's size-bar. Context rows are unchanged, so they do not count.
pub(crate) fn hunk_magnitude(hunks: &[Hunk]) -> u32 {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "changed-line count cannot approach u32::MAX"
    )]
    let n = hunks
        .iter()
        .flat_map(|h| h.rows.iter())
        .filter(|r| matches!(r, Row::Del(_) | Row::Add(_)))
        .count() as u32;
    n
}

/// One grouped hunk of a whole-file diff, carried by a [`crate::bus::card::Mark::Diff`].
///
/// A flat unified list of [`Row`]s — context,
/// deletions, and insertions interleaved exactly as `similar`'s grouped ops
/// yield them.
/// `start` is the 1-indexed original line of the hunk's first
/// row; the sink walks the rows from there, advancing an old- and a
/// new-side counter — a `Context` advances both, a `Del` advances the old
/// counter (and keeps its pre-edit number), an `Add` advances the new
/// counter (and takes its post-edit number).
#[derive(Clone, Debug, Serialize)]
pub struct Hunk {
    pub start: u32,
    pub rows: Vec<Row>,
}

/// One run of a diff row's text: a contiguous slice flagged `emph` when it is
/// the part that actually changed against the row's paired line — the
/// intra-line word diff `similar` computes.
///
/// A context row, and the unchanged
/// stretches that surround a change on a del/add row, carry `emph: false`.
#[derive(Clone, Debug, Serialize)]
pub struct Seg {
    pub emph: bool,
    pub text: String,
}

impl Seg {
    /// A whole, unemphasised run — the shape a context row carries and the
    /// default a plainly-constructed del/add row falls back to.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            emph: false,
            text: text.into(),
        }
    }
}

/// One row of a [`Hunk`]'s unified line list: unchanged context, a removed
/// line, or an inserted line.
///
/// Each carries its text as a run of [`Seg`]ments
/// so a del/add can mark the words that changed against its paired line; a
/// context row is a single unemphasised segment.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "tag", content = "segs", rename_all = "snake_case")]
pub enum Row {
    Context(Vec<Seg>),
    Del(Vec<Seg>),
    Add(Vec<Seg>),
}

impl Row {
    /// The row's segments, whatever its kind.
    pub fn segs(&self) -> &[Seg] {
        match self {
            Self::Context(s) | Self::Del(s) | Self::Add(s) => s,
        }
    }

    /// The row's full text — its segments concatenated, dropping the
    /// inline-emphasis distinction (the plain-text/headless rendering).
    pub fn text(&self) -> String {
        self.segs().iter().map(|s| s.text.as_str()).collect()
    }
}

/// Compute the whole-file line-level diff of `old` vs `new`, grouped into
/// hunks with ±2 lines of context.  Each hunk's `start` is the 1-indexed
/// original line of its first row, and its rows are the unified context /
/// deletion / insertion list `similar` yields.
///
/// The sole caller is [`crate::bus::card::io::write_preview`], so every committed write reaches
/// this same diff through the one [`crate::bus::card::value_to_io`]/[`crate::bus::card::io_card`] decode,
/// whatever wrote it: a `>` redirect (core composes the `` `io `` value
/// itself) or `edit-hash`/`edit-replace` (`shell_eval/builtins.rs`'s
/// [`crate::shell_eval::builtins::surface_write`] composes the identical value by hand, since those
/// builtins write below the redirect frame and so must self-report).
pub(crate) fn whole_file_hunks(old: &str, new: &str) -> Vec<Hunk> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(2) {
        let first = group.first().expect("grouped_ops yields non-empty groups");
        #[allow(
            clippy::cast_possible_truncation,
            reason = "diff line index cannot approach u32::MAX"
        )]
        let start = first.old_range().start as u32 + 1;
        let mut rows = Vec::new();
        for op in &group {
            // `iter_inline_changes` is what buys the per-row [`Seg`] shape:
            // an `Equal` op still yields one whole (unemphasised) segment.
            for change in diff.iter_inline_changes(op) {
                let mut segs: Vec<Seg> = change
                    .iter_strings_lossy()
                    .map(|(emph, text)| Seg {
                        emph,
                        text: text.into_owned(),
                    })
                    .collect();
                // `from_lines` keeps a trailing `\n` on each row's final
                // segment; strip it so the row carries a bare line, matching
                // how `rows_of` splits the file.  If stripping leaves that
                // segment empty (a line that was pure `\n`), drop it outright.
                if let Some(last) = segs.last_mut() {
                    if let Some(bare) = last.text.strip_suffix('\n') {
                        last.text = bare.to_string();
                    }
                    if last.text.is_empty() {
                        segs.pop();
                    }
                }
                rows.push(match change.tag() {
                    ChangeTag::Equal => Row::Context(segs),
                    ChangeTag::Delete => Row::Del(segs),
                    ChangeTag::Insert => Row::Add(segs),
                });
            }
        }
        hunks.push(Hunk { start, rows });
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our wiring of `similar`'s inline changes into [`Row`]s: a changed line
    /// threads through as segments that concatenate back to the original line
    /// (trailing newline stripped) and carry *both* an emphasised and an
    /// unemphasised run, so the emph distinction the renderer needs survives.
    /// *Which* words `similar` flags is its concern, not ours, so we don't
    /// assert the boundary.
    #[test]
    fn whole_file_hunks_threads_inline_segments() {
        let hunks = whole_file_hunks("alpha\nthe quick brown fox\n", "alpha\nthe quick red fox\n");
        let rows: Vec<&Row> = hunks.iter().flat_map(|h| h.rows.iter()).collect();
        let find = |want: fn(&Row) -> bool| *rows.iter().find(|r| want(r)).expect("the row");

        // The shared `alpha` line maps to a context row of one unemphasised
        // segment — our `Equal → Context` mapping.
        let ctx = find(|r| matches!(r, Row::Context(_)));
        assert_eq!(ctx.text(), "alpha");
        assert!(ctx.segs().iter().all(|s| !s.emph));

        // The edited line round-trips on each side, with the `\n` `from_lines`
        // carries stripped, and keeps both an emphasised and an unchanged run.
        for (row, text) in [
            (find(|r| matches!(r, Row::Del(_))), "the quick brown fox"),
            (find(|r| matches!(r, Row::Add(_))), "the quick red fox"),
        ] {
            assert_eq!(row.text(), text);
            assert!(!row.segs().iter().any(|s| s.text.ends_with('\n')));
            assert!(row.segs().iter().any(|s| s.emph), "an emphasised run");
            assert!(row.segs().iter().any(|s| !s.emph), "an unchanged run");
        }
    }
}
