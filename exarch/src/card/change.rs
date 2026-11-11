//! What a write or an edit did to one file: the one fact every file mutation
//! lands as, whichever door it came through.

use ral_core::shell::runtime::{Observed, WriteOutcome};
use serde::{Deserialize, Serialize};

use super::diff::Diff;
use super::{Card, Mark, Role, Span};

/// One file mutation: where, how it settled, and — where both sides were
/// known as text — the change itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub path: String,
    pub outcome: WriteOutcome,
    pub diff: Option<Diff>,
}

impl Change {
    /// The change a redirect's write made, or `None` for any other
    /// observation.  No diff without both sides: an unknown before-image must
    /// not read as a creation.
    pub(crate) fn of(what: &Observed) -> Option<Self> {
        let Observed::Write(w) = what else {
            return None;
        };
        Some(Self {
            path: w.path.clone(),
            outcome: w.outcome,
            diff: text(w.old_bytes.as_deref())
                .zip(text(w.new_bytes.as_deref()))
                .map(|(old, new)| Diff::between(old, new)),
        })
    }

    /// Changed lines, kept or cut.
    pub(crate) fn lines(&self) -> u32 {
        self.diff.as_ref().map_or(0, |d| d.added + d.removed)
    }
}

/// A snapshot as text: `None` when there is none, or it is not UTF-8.
fn text(side: Option<&[u8]>) -> Option<&str> {
    std::str::from_utf8(side?).ok()
}

/// How a write settled, as one word roled by its level.
pub(crate) fn settled(outcome: WriteOutcome) -> Span {
    match outcome {
        WriteOutcome::Committed => Span::new(Role::Ok, "committed"),
        WriteOutcome::Aborted => Span::new(Role::Warn, "aborted"),
        WriteOutcome::Failed => Span::new(Role::Bad, "failed"),
    }
}

/// `write <path> <outcome>`: all a change with no diff can say.
pub(crate) fn heading(path: &str, outcome: WriteOutcome) -> Vec<Span> {
    vec![
        Span::new(Role::Muted, "write "),
        Span::new(Role::Path, path),
        Span::plain(" "),
        settled(outcome),
    ]
}

/// A change as a card, for a printer that draws one fact at a time.
///
/// Its diff names the file, or else its heading does.  A write that did not
/// commit says so above whatever it left, and a diff cut at the source says
/// what it cut.
pub fn change_card(change: &Change) -> Card {
    let heading = || Mark::Text {
        spans: heading(&change.path, change.outcome),
    };
    let Some(diff) = &change.diff else {
        return Card(vec![heading()]);
    };
    let unsettled = (change.outcome != WriteOutcome::Committed).then(heading);
    let cut = (diff.cut() > 0).then(|| Mark::Text {
        spans: vec![Span::new(
            Role::Muted,
            format!("… {} more changed lines", diff.cut()),
        )],
    });
    let diff = Mark::Diff {
        path: change.path.clone(),
        hunks: diff.hunks.clone(),
    };
    Card(unsettled.into_iter().chain([diff]).chain(cut).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::data::Bytes;
    use ral_core::fact::Write;
    use ral_core::ir::WriteMode;

    fn write(old: Option<&[u8]>, new: Option<&[u8]>, outcome: WriteOutcome) -> Observed {
        Observed::Write(Write {
            path: "a.txt".into(),
            mode: WriteMode::Write,
            outcome,
            new_bytes: new.map(Bytes::from),
            old_bytes: old.map(Bytes::from),
        })
    }

    #[test]
    fn a_diff_needs_both_sides_as_text() {
        let diff = |old, new| {
            Change::of(&write(old, new, WriteOutcome::Committed))
                .unwrap()
                .diff
        };
        let created = diff(Some(b""), Some(b"x\n")).expect("an empty before-image is a creation");
        assert_eq!((created.added, created.removed), (1, 0));
        assert!(diff(None, Some(b"x\n")).is_none(), "unknown before-image");
        assert!(diff(Some(b"x\n"), None).is_none(), "unknown after-image");
        assert!(
            diff(Some(&[0xff, 0xfe]), Some(b"x\n")).is_none(),
            "binary before"
        );
        assert!(
            diff(Some(b"x\n"), Some(&[0xff, 0xfe])).is_none(),
            "binary after"
        );
    }

    fn card_of(old: Option<&[u8]>, new: Option<&[u8]>, outcome: WriteOutcome) -> Card {
        change_card(&Change::of(&write(old, new, outcome)).unwrap())
    }

    #[test]
    fn an_unsettled_write_is_headed_above_its_diff() {
        let card = card_of(Some(b"a\n"), Some(b"b\n"), WriteOutcome::Aborted);
        let [Mark::Text { spans }, Mark::Diff { .. }] = card.marks() else {
            panic!("a heading, then the diff: {card:?}");
        };
        assert_eq!(spans.last().and_then(|s| s.role), Some(Role::Warn));
    }

    #[test]
    fn a_change_without_a_diff_is_only_its_heading() {
        let card = card_of(None, Some(b"b\n"), WriteOutcome::Committed);
        assert!(matches!(card.marks(), [Mark::Text { .. }]));
    }

    #[test]
    fn a_cut_diff_ends_in_a_note_of_what_it_cut() {
        let big = "x\n".repeat(2500);
        let change = Change::of(&write(
            Some(b""),
            Some(big.as_bytes()),
            WriteOutcome::Committed,
        ))
        .unwrap();
        let cut = change.diff.as_ref().unwrap().cut();
        assert!(cut > 0);
        let card = change_card(&change);
        let [Mark::Diff { .. }, Mark::Text { spans }] = card.marks() else {
            panic!("the diff, then a cut note: {card:?}");
        };
        assert!(spans[0].text.contains(&cut.to_string()));
    }
}
