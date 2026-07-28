//! The diff mark's interior: a [`Hunk`] is a run of [`Row`]s, and a [`Row`] a
//! run of [`Seg`]ments carrying the word-level emphasis `similar` picks out.
//! [`whole_file_hunks`] is the one place a pair of texts becomes this shape.

use serde::Serialize;

/// Total changed lines across `hunks`, context counting for nothing — the
/// magnitude `Card::magnitude` and `tui::line`'s size bar both read.
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

/// One grouped hunk of a whole-file diff, carried by a `Mark::Diff`: context,
/// deletions and insertions interleaved as one unified list of [`Row`]s.
///
/// `start` is the 1-indexed *original* line of the first row; `tui::line`
/// numbers the gutter by walking from there, advancing an old- and a new-side
/// counter separately.
#[derive(Clone, Debug, Serialize)]
pub struct Hunk {
    pub start: u32,
    pub rows: Vec<Row>,
}

/// One run of a row's text, `emph` when it is the part that changed against
/// the row's paired line — `similar`'s intra-line word diff.
#[derive(Clone, Debug, Serialize)]
pub struct Seg {
    pub emph: bool,
    pub text: String,
}

impl Seg {
    /// A whole, unemphasised run — what a context row carries.
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            emph: false,
            text: text.into(),
        }
    }
}

/// One row of a [`Hunk`]'s unified list — context, a removed line, or an
/// inserted line — carrying its text as a run of [`Seg`]ments.
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

    /// The row's segments concatenated, dropping the emphasis distinction.
    pub fn text(&self) -> String {
        self.segs().iter().map(|s| s.text.as_str()).collect()
    }
}

/// The whole-file line diff of `old` vs `new`, grouped into hunks with ±2
/// lines of context.
///
/// Called only from `write_preview` in the sibling `io` module, so every
/// committed write — a `>` redirect, `edit-hash`, `edit-replace` — is diffed
/// here and nowhere else.
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
            // An `Equal` op still yields one whole, unemphasised segment.
            for change in diff.iter_inline_changes(op) {
                let mut segs: Vec<Seg> = change
                    .iter_strings_lossy()
                    .map(|(emph, text)| Seg {
                        emph,
                        text: text.into_owned(),
                    })
                    .collect();
                // `from_lines` keeps a trailing `\n` on each row's final
                // segment; strip it so the row carries a bare line, and drop
                // the segment outright if that leaves it empty.
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

    /// A changed line threads through as segments that rejoin to the original
    /// line, newline stripped, carrying both an emphasised and an unemphasised
    /// run.  *Which* words `similar` flags is its business, not ours.
    #[test]
    fn whole_file_hunks_threads_inline_segments() {
        let hunks = whole_file_hunks("alpha\nthe quick brown fox\n", "alpha\nthe quick red fox\n");
        let rows: Vec<&Row> = hunks.iter().flat_map(|h| h.rows.iter()).collect();
        let find = |want: fn(&Row) -> bool| *rows.iter().find(|r| want(r)).expect("the row");

        let ctx = find(|r| matches!(r, Row::Context(_)));
        assert_eq!(ctx.text(), "alpha");
        assert!(ctx.segs().iter().all(|s| !s.emph));

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
