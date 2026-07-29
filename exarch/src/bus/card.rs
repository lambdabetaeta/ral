//! The render document the `surface` builtin carries: an ordered stack of marks
//! a ral kit composes, decoded once by [`value_to_card`] and interpreted by
//! `tui::line::render_card`.
//!
//! Cards are open — compose marks in ral, zero Rust per card — but the mark
//! set is closed, so the renderer is total.
//!
//! The kit declares data and its level of measurement, never its appearance: a
//! [`Span`] names a nominal [`Role`], a [`Measure`] or [`Mark::Diff`] carries a
//! magnitude, and the one table binding role to ink lives in the renderer, so
//! magnitude can never reach hue.
//!
//! [`decode`] reads a kit's `` `card `` value into this model; [`diff`] and
//! [`value`] are the substrates every decoder shares; [`io`], [`done`] and
//! [`notice`] each decode one class of event core surfaces.

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

/// The closed nominal role set — the identity channel a [`Span`] may carry.
/// An unrecognised tag degrades to plain ink rather than dropping the span.
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

/// A run of text optionally carrying a nominal [`Role`] — never a magnitude,
/// which is [`Measure`]'s and [`Mark::Diff`]'s work.
#[derive(Clone, Debug, Serialize)]
pub struct Span {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    pub text: String,
}

impl Span {
    pub(crate) fn new(role: Role, text: impl Into<String>) -> Self {
        Self {
            role: Some(role),
            text: text.into(),
        }
    }

    pub(crate) fn plain(text: impl Into<String>) -> Self {
        Self {
            role: None,
            text: text.into(),
        }
    }
}

/// The quantitative mark `[label, value, max?, unit?]`: a bounded magnitude
/// (`max` present) renders as a proportional fill bar, an unbounded one as a
/// `log2` size bar.
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
    /// The unlabelled `value[/max][unit]` readout, shared by
    /// [`FieldVal::plain`] and [`summary_line`]'s measure arm.
    fn readout(&self) -> String {
        let bound = self.max.map(|mx| format!("/{mx}")).unwrap_or_default();
        format!(
            "{}{bound}{}",
            self.value,
            self.unit.as_deref().unwrap_or("")
        )
    }
}

/// A [`Mark::Fields`] row's value: inline spans or a nested [`Measure`] — the
/// one place a mark nests inside another.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldVal {
    Inline(Vec<Span>),
    Measure(Measure),
}

impl FieldVal {
    /// The value's plain text — shared by [`summary_line`] and the headless
    /// stderr condenser in `exarch/src/headless.rs`.
    pub(crate) fn plain(&self) -> String {
        match self {
            Self::Inline(spans) => spans.iter().map(|s| s.text.as_str()).collect(),
            Self::Measure(m) => m.readout(),
        }
    }
}

/// One `(label, value)` row of a [`Mark::Fields`] matrix.
#[derive(Clone, Debug, Serialize)]
pub struct Field {
    pub label: String,
    pub value: FieldVal,
}

/// One mark on the plane — closed and small so the renderer is total, stacked
/// openly into a [`Card`] in ral.
///
/// A kit may surface every variant but `Listing`, the host-composed
/// gutter-numbered head of a freshly-written file; `Raw` is pre-formed bytes
/// appended verbatim.
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

/// An ordered stack of [`Mark`]s rendered top-to-bottom as one scrollback
/// block — the document `surface` carries.
#[derive(Clone, Debug, Serialize)]
pub struct Card(pub Vec<Mark>);

impl Card {
    pub(crate) fn marks(&self) -> &[Mark] {
        &self.0
    }

    /// True when the card carries a [`Mark::Diff`] — the only content that
    /// earns graded disclosure; anything else is chrome, rendered whole.
    pub(crate) fn has_diff(&self) -> bool {
        self.0.iter().any(|m| matches!(m, Mark::Diff { .. }))
    }

    /// Total changed lines across the card's `diff` marks, `None` when it has
    /// none.  The rail's value step and the matrix's lines-touched readout
    /// both rank cards by this.
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

    /// The `(path, hunks)` of a lone `diff` mark — the key consecutive
    /// same-path diff cards merge on, so one file reads as one block the way a
    /// unified diff presents it.  `None` for any richer card.
    pub(crate) fn single_diff(&self) -> Option<(&str, &[Hunk])> {
        match self.0.as_slice() {
            [Mark::Diff { path, hunks }] => Some((path, hunks)),
            _ => None,
        }
    }

    /// Consume a lone `diff` card into its owned `(path, hunks)` for the
    /// patch-aggregation buffer.
    ///
    /// # Errors
    /// `Err(self)` hands a richer card back untouched, for the caller to push
    /// as its own block.
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

/// A [`Card`] as one line — the digest the periodic nudge shows the model of
/// its pinned state, where the TUI's framed rendering is out of reach.
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
