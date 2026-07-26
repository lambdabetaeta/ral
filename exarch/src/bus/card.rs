//! The render document the `surface` builtin carries.
//!
//! A [`Card`] is an ordered stack of Bertin *marks* the kit composes
//! entirely in ral; exarch decodes it once here ([`value_to_card`]) into
//! this closed Rust model and renders it through one generic interpreter
//! (`tui::line::render_card`).  The *set of cards* is open — compose marks
//! in ral, zero Rust per card — while the *set of marks* stays closed and
//! small, so the renderer is total and reflow / disclosure / aggregation /
//! the rendered `user.log` all keep working.
//!
//! The discipline is Bertin's: the kit declares **data and its level of
//! measurement, never its appearance**.  A [`Span`] carries a nominal
//! [`Role`] (identity → hue/shape); a [`Measure`] or [`Mark::Diff`] carries
//! a magnitude (ordered → size/value/grain).  The one binding table lives
//! in the renderer, so the kit *cannot* put magnitude on hue: the encoding
//! is correct by construction.
//!
//! The mark vocabulary and the [`Card`] itself live here; the rest lives in
//! a sibling module per concern — a decoder per surface class, plus the two
//! substrates they share:
//!
//! - [`diff`] — the diff mark's interior: hunks, rows, segments, and the
//!   whole-file diff that produces them.
//! - [`value`] — the field readers every decoder pulls a ral `Value` through.
//! - [`decode`] — a kit's `` `card `` value into this closed model, degrading
//!   rather than raising on anything unrecognised.
//! - [`io`] — the structural read/write/exec/grep events core surfaces onto
//!   the bus.
//! - [`done`] — how a detached `spawn` worker settled.
//! - [`notice`] — core's own ready-boundary housekeeping: reaps and prunes.

use serde::Serialize;

mod decode;
mod diff;
mod done;
mod io;
mod notice;
#[cfg(test)]
mod testkit;
mod value;

pub use diff::{Hunk, Row, Seg};
pub use done::DoneOutcome;
pub use io::{ExecOutcome, IoEvent, WriteMode, WriteOutcome};
pub use notice::Notice;

pub(crate) use decode::{value_to_card, value_to_pin};
pub(crate) use diff::hunk_magnitude;
pub(crate) use done::{done_card, value_to_done};
pub(crate) use io::{ObservationKind, execs_card, greps_card, io_card, reads_card, value_to_io};
pub(crate) use notice::{notice_card, services_pin_card, value_to_notice};

/// The closed nominal role set — the *selective* (identity) channel a
/// [`Span`] may carry.
///
/// The renderer holds the one binding table mapping
/// each role to a hue/shape; the kit names a role, never a colour, so
/// identity can never masquerade as a magnitude.  An unknown role tag
/// renders as plain ink rather than dropping.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Path,
    Code,
    Ok,
    Warn,
    Bad,
    Muted,
    Strong,
}

impl Role {
    /// Parse a nominal role tag; `None` for an unrecognised role.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "path" => Self::Path,
            "code" => Self::Code,
            "ok" => Self::Ok,
            "warn" => Self::Warn,
            "bad" => Self::Bad,
            "muted" => Self::Muted,
            "strong" => Self::Strong,
            _ => return None,
        })
    }
}

/// A run of text optionally carrying a nominal [`Role`].
///
/// A heading is
/// just a `Strong` span; a path is a `Path` span.  A span never carries a
/// magnitude — that is the job of [`Measure`] and [`Mark::Diff`].
#[derive(Clone, Debug, Serialize)]
pub struct Span {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    pub text: String,
}

impl Span {
    /// A roled span carrying `text`.
    pub(crate) fn new(role: Role, text: impl Into<String>) -> Self {
        Self {
            role: Some(role),
            text: text.into(),
        }
    }

    /// A roleless (plain-ink) span carrying `text`.
    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Self {
            role: None,
            text: text.into(),
        }
    }
}

/// The quantitative mark `[label, value, max?, unit?]`, rendered with the
/// two ordered Bertin variables — size (a bar) and value (lightness).
///
/// A
/// bounded magnitude (`max` present) reads as a proportional fill (the old
/// progress meter); an unbounded one (`max` absent) reads as a `log2` size
/// bar (the old header size-bar).  Both apply the value ramp, so a larger
/// magnitude reads brighter as well as fuller.
#[derive(Clone, Debug, Serialize)]
pub struct Measure {
    pub label: String,
    pub value: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

impl Measure {
    /// The `value[/max][unit]` readout, unlabelled — shared by
    /// [`FieldVal::plain`] and [`summary_line`]'s `Mark::Measure` arm.
    fn readout(&self) -> String {
        let bound = self.max.map(|mx| format!("/{mx}")).unwrap_or_default();
        format!(
            "{}{bound}{}",
            self.value,
            self.unit.as_deref().unwrap_or("")
        )
    }
}

/// A value in a [`Mark::Fields`] row's shared value column: a run of inline
/// spans (`text`) or a nested [`Measure`] — the one composability rule
/// (marks nest in a field's value) at the field scale.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldVal {
    Inline(Vec<Span>),
    Measure(Measure),
}

impl FieldVal {
    /// The value's plain text: inline spans concatenated, or a measure's
    /// `value[/max][unit]` readout.  Shared by the [`summary_line`] rail
    /// summary and the headless stderr condenser.
    pub(crate) fn plain(&self) -> String {
        match self {
            Self::Inline(spans) => spans.iter().map(|s| s.text.as_str()).collect(),
            Self::Measure(m) => m.readout(),
        }
    }
}

/// One `(label, value)` row of a [`Mark::Fields`] matrix — Bertin's
/// selective alignment in miniature, every value landing in one shared
/// label column.
#[derive(Clone, Debug, Serialize)]
pub struct Field {
    pub label: String,
    pub value: FieldVal,
}

/// One Bertin mark on the plane.  Closed and small so the renderer is
/// total; stacked openly into a [`Card`] in ral.
///
/// - [`Mark::Text`] — the *qualitative* mark: a run of optionally-roled spans.
/// - [`Mark::Measure`] — the *quantitative* mark (size + value).
/// - [`Mark::Fields`] — the *matrix* mark: an aligned `(label, value)` table.
/// - [`Mark::Diff`] — the *dense composite* mark (size + grain + value + shape).
/// - [`Mark::Listing`] — a *numbered source listing*: the head of a written
///   file, gutter-numbered and syntax-lit but one-sided (no `+`/`-`).
/// - [`Mark::Raw`] — *un-encoded ink*: pre-formed bytes appended verbatim.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "mark", rename_all = "snake_case")]
pub enum Mark {
    Text { spans: Vec<Span> },
    Measure(Measure),
    Fields { rows: Vec<Field> },
    Diff { path: String, hunks: Vec<Hunk> },
    Listing { bytes: Vec<u8>, more: bool },
    Raw { bytes: Vec<u8> },
}

/// An ordered stack of [`Mark`]s rendered top-to-bottom on one scrollback
/// block — the render document `surface` carries, composed in ral.
#[derive(Clone, Debug, Serialize)]
pub struct Card(pub Vec<Mark>);

impl Card {
    /// The card's marks, in plane order.
    pub(crate) fn marks(&self) -> &[Mark] {
        &self.0
    }

    /// True when the card carries at least one [`Mark::Diff`] — the only
    /// content that earns graded disclosure (L1 header ↔ L3 full).  A card
    /// of only `text`/`fields`/`measure`/`raw` is chrome-level (L3-only).
    pub(crate) fn has_diff(&self) -> bool {
        self.0.iter().any(|m| matches!(m, Mark::Diff { .. }))
    }

    /// The card's magnitude — total changed lines summed across its `diff`
    /// marks, or `None` when it carries no diff.  The rail's value-step and
    /// the matrix's per-agent size readout both read this.
    pub(crate) fn magnitude(&self) -> Option<u32> {
        let mut any = false;
        let total = self
            .0
            .iter()
            .filter_map(|m| match m {
                Mark::Diff { hunks, .. } => {
                    any = true;
                    Some(hunk_magnitude(hunks))
                }
                _ => None,
            })
            .sum();
        any.then_some(total)
    }

    /// When the card is exactly one `diff` mark, its `(path, hunks)` — the
    /// aggregation key consecutive same-path diff cards merge on, mirroring
    /// a unified diff's single per-file block.  `None` for any richer card.
    pub(crate) fn single_diff(&self) -> Option<(&str, &[Hunk])> {
        match self.0.as_slice() {
            [Mark::Diff { path, hunks }] => Some((path, hunks)),
            _ => None,
        }
    }

    /// Consume a single-`diff` card into its owned `(path, hunks)` for the
    /// patch-aggregation buffer; `Err(self)` hands a richer card back
    /// untouched so the caller can push it as its own block.
    ///
    /// # Errors
    /// Returns `Err(self)` if the card is not exactly one `diff` mark.
    pub(crate) fn into_single_diff(self) -> Result<(String, Vec<Hunk>), Self> {
        if self.single_diff().is_some() {
            let Self(mut marks) = self;
            match marks.pop() {
                Some(Mark::Diff { path, hunks }) => Ok((path, hunks)),
                _ => unreachable!("single_diff checked exactly one diff mark"),
            }
        } else {
            Err(self)
        }
    }
}

/// A [`Card`] as a compact one-line summary — the session-layer digest the
/// nudge facility shows when reminding the model of its pinned state, where the
/// TUI's framed rendering is out of reach.
///
/// Text marks concatenate their span
/// runs (whitespace collapsed); a measure reads `label value/maxunit`; a fields
/// matrix reads `label value` pairs; a diff names its path; a listing and raw
/// ink are both their bytes, lossily.  Marks join with a space.
pub(crate) fn summary_line(card: &Card) -> String {
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut parts: Vec<String> = Vec::new();
    for mark in card.marks() {
        let part = match mark {
            Mark::Text { spans } => collapse(
                &spans
                    .iter()
                    .map(|s| {
                        if s.role == Some(Role::Strong) {
                            format!("{}: ", s.text)
                        } else {
                            s.text.clone()
                        }
                    })
                    .collect::<String>(),
            ),
            Mark::Measure(m) => format!("{} {}", m.label, m.readout()),
            Mark::Fields { rows } => rows
                .iter()
                .map(|f| format!("{} {}", f.label, f.value.plain()))
                .collect::<Vec<_>>()
                .join(", "),
            Mark::Diff { path, .. } => format!("diff {path}"),
            Mark::Listing { bytes, .. } | Mark::Raw { bytes } => {
                collapse(&String::from_utf8_lossy(bytes))
            }
        };
        if !part.is_empty() {
            parts.push(part);
        }
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A card serialises to a structured mark tree — with each mark internally
    /// tagged and a `raw` mark carrying its bytes.  Only `raw` is opaque, and
    /// honestly so.
    #[test]
    fn serialises_to_a_structured_mark_tree() {
        let card = Card(vec![
            Mark::Text {
                spans: vec![Span {
                    role: Some(Role::Ok),
                    text: "done".into(),
                }],
            },
            Mark::Measure(Measure {
                label: "tasks".into(),
                value: 3,
                max: Some(12),
                unit: None,
            }),
            Mark::Raw {
                bytes: vec![0xff, b'h'],
            },
        ]);
        let v = serde_json::to_value(&card).expect("a card serialises");
        let marks = v.as_array().expect("a card is a JSON array of marks");
        assert_eq!(marks[0]["mark"], "text");
        assert_eq!(marks[0]["spans"][0]["role"], "ok");
        assert_eq!(marks[1]["mark"], "measure");
        assert_eq!(marks[1]["value"], 3);
        assert_eq!(marks[1]["max"], 12);
        assert_eq!(marks[2]["mark"], "raw");
        assert_eq!(marks[2]["bytes"], serde_json::json!([255, 104]));
    }

    /// `single_diff` keys aggregation: exactly one diff mark yields its
    /// path + hunks; a richer card does not.
    #[test]
    fn single_diff_keys_aggregation() {
        let one = Card(vec![Mark::Diff {
            path: "a.rs".into(),
            hunks: vec![Hunk {
                start: 1,
                rows: vec![
                    Row::Del(vec![Seg::plain("x")]),
                    Row::Add(vec![Seg::plain("y")]),
                    Row::Add(vec![Seg::plain("z")]),
                ],
            }],
        }]);
        assert_eq!(one.single_diff().map(|(p, _)| p), Some("a.rs"));
        assert_eq!(one.magnitude(), Some(3));
        assert!(one.has_diff());
        let rich = Card(vec![
            Mark::Text { spans: vec![] },
            Mark::Diff {
                path: "a.rs".into(),
                hunks: vec![],
            },
        ]);
        assert!(rich.single_diff().is_none());
        assert!(rich.has_diff());
        let plain = Card(vec![Mark::Text { spans: vec![] }]);
        assert_eq!(plain.magnitude(), None);
        assert!(!plain.has_diff());
    }
}
