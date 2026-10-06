//! The diff mark's interior: a [`Hunk`] is a run of [`Row`]s, and a [`Row`] a
//! run of [`Seg`]ments carrying the word-level emphasis `similar` picks out.
//! [`Diff::between`] is the one place a pair of texts becomes this shape.

use serde::{Deserialize, Serialize};
use similar::{ChangeTag, DiffTag, InlineChange, TextDiff};
use std::time::{Duration, Instant};

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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Hunk {
    pub start: u32,
    pub rows: Vec<Row>,
}

/// One run of a row's text, `emph` when it is the part that changed against
/// the row's paired line — `similar`'s intra-line word diff.
#[derive(Clone, Debug, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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

/// A line diff cut at the source: hunks with two lines of context, keeping
/// at most [`KEPT_ROWS`] rows, and every changed line counted, kept or not.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Diff {
    pub hunks: Vec<Hunk>,
    pub added: u32,
    pub removed: u32,
}

/// Rows a diff keeps, context included: past them a change is counted, not
/// carried.
const KEPT_ROWS: usize = 2000;

/// How long a diff may search for the smallest change before settling for a
/// larger one.
const PATIENCE: Duration = Duration::from_millis(250);

impl Diff {
    /// The diff of `old` against `new` — the one place two texts become hunks.
    pub(crate) fn between(old: &str, new: &str) -> Self {
        let deadline = Instant::now() + PATIENCE;
        let diff = TextDiff::configure()
            .deadline(deadline)
            .diff_lines(old, new);
        let (added, removed) = diff
            .ops()
            .iter()
            .filter(|op| op.tag() != DiffTag::Equal)
            .fold((0, 0), |(a, r), op| {
                (a + op.new_range().len(), r + op.old_range().len())
            });
        let mut room = KEPT_ROWS;
        let mut hunks = Vec::new();
        for group in diff.grouped_ops(2) {
            if room == 0 {
                break;
            }
            let rows: Vec<Row> = group
                .iter()
                .flat_map(|op| diff.iter_inline_changes_deadline(op, Some(deadline)))
                .take(room)
                .map(|change| row(&change))
                .collect();
            room -= rows.len();
            hunks.push(Hunk {
                start: saturating(group[0].old_range().start + 1),
                rows,
            });
        }
        Self {
            hunks,
            added: saturating(added),
            removed: saturating(removed),
        }
    }

    /// Changed lines counted but not kept.
    pub(crate) fn cut(&self) -> u32 {
        (self.added + self.removed).saturating_sub(hunk_magnitude(&self.hunks))
    }
}

fn saturating(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// One line of a diff as a row, its trailing newline dropped so the row
/// carries the bare line.
fn row(change: &InlineChange<'_, str>) -> Row {
    let mut segs: Vec<Seg> = change
        .iter_strings_lossy()
        .map(|(emph, text)| Seg {
            emph,
            text: text.into_owned(),
        })
        .collect();
    if let Some(last) = segs.last_mut()
        && last.text.ends_with('\n')
    {
        last.text.pop();
    }
    if segs.last().is_some_and(|s| s.text.is_empty()) {
        segs.pop();
    }
    match change.tag() {
        ChangeTag::Equal => Row::Context(segs),
        ChangeTag::Delete => Row::Del(segs),
        ChangeTag::Insert => Row::Add(segs),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A changed line threads through as segments that rejoin to the original
    /// line, newline stripped, carrying both an emphasised and an unemphasised
    /// run.  *Which* words `similar` flags is its business, not ours.
    #[test]
    fn a_changed_line_threads_inline_segments() {
        let diff = Diff::between("alpha\nthe quick brown fox\n", "alpha\nthe quick red fox\n");
        let rows: Vec<&Row> = diff.hunks.iter().flat_map(|h| h.rows.iter()).collect();
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

    /// However much was written, a diff keeps [`KEPT_ROWS`] rows and counts
    /// every changed line.
    #[test]
    fn a_huge_change_is_cut_at_the_source_and_counted_whole() {
        let lines = KEPT_ROWS + 500;
        let diff = Diff::between("", &"x\n".repeat(lines));
        let kept: usize = diff.hunks.iter().map(|h| h.rows.len()).sum();
        assert_eq!(kept, KEPT_ROWS);
        assert_eq!((diff.added, diff.removed), (saturating(lines), 0));
        assert_eq!(diff.cut(), 500);
    }
}
