//! What a write or an edit did to one file: the one fact every file mutation
//! lands as, whichever door it came through.

use ral_core::types::{Observed, WriteOutcome};
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
        let Observed::Write {
            path,
            outcome,
            new_bytes,
            old_bytes,
            ..
        } = what
        else {
            return None;
        };
        Some(Self {
            path: path.clone(),
            outcome: *outcome,
            diff: text(old_bytes.as_deref())
                .zip(text(new_bytes.as_deref()))
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
