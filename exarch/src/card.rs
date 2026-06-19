//! The render document the `surface` builtin carries.
//!
//! A [`Card`] is an ordered stack of Bertin *marks* the kit composes
//! entirely in ral; exarch decodes it once here ([`value_to_card`]) into
//! this closed Rust model and renders it through one generic interpreter
//! (`tui::line::render_card`).  The *set of cards* is open — compose marks
//! in ral, zero Rust per card — while the *set of marks* stays closed and
//! small, so the renderer is total and reflow / disclosure / aggregation /
//! the structured `transcript.jsonl` log all keep working.
//!
//! The discipline is Bertin's: the kit declares **data and its level of
//! measurement, never its appearance**.  A [`Span`] carries a nominal
//! [`Role`] (identity → hue/shape); a [`Measure`] or [`Mark::Diff`] carries
//! a magnitude (ordered → size/value/grain).  The one binding table lives
//! in the renderer, so the kit *cannot* put magnitude on hue: the encoding
//! is correct by construction.  See
//! `docs/ral-wiki/decisions/260619_surface-carries-documents.md`.

use crate::bus::Hunk;
use ral_core::Value as RalValue;
use serde::Serialize;

/// The closed nominal role set — the *selective* (identity) channel a
/// [`Span`] may carry.  The renderer holds the one binding table mapping
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
    pub fn parse(s: &str) -> Option<Self> {
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

/// A run of text optionally carrying a nominal [`Role`].  A heading is
/// just a `Strong` span; a path is a `Path` span.  A span never carries a
/// magnitude — that is the job of [`Measure`] and [`Mark::Diff`].
#[derive(Clone, Debug, Serialize)]
pub struct Span {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    pub text: String,
}

/// The quantitative mark `[label, value, max?, unit?]`, rendered with the
/// two ordered Bertin variables — size (a bar) and value (lightness).  A
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

/// A value in a [`Mark::Fields`] row's shared value column: a run of inline
/// spans (`text`) or a nested [`Measure`] — the one composability rule
/// (marks nest in a field's value) at the field scale.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldVal {
    Inline(Vec<Span>),
    Measure(Measure),
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
/// - [`Mark::Raw`] — *un-encoded ink*: pre-formed bytes appended verbatim.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "mark", rename_all = "snake_case")]
pub enum Mark {
    Text { spans: Vec<Span> },
    Measure(Measure),
    Fields { rows: Vec<Field> },
    Diff { path: String, hunks: Vec<Hunk> },
    Raw { bytes: Vec<u8> },
}

/// An ordered stack of [`Mark`]s rendered top-to-bottom on one scrollback
/// block — the render document `surface` carries, composed in ral.
#[derive(Clone, Debug, Serialize)]
pub struct Card(pub Vec<Mark>);

impl Card {
    /// The card's marks, in plane order.
    pub fn marks(&self) -> &[Mark] {
        &self.0
    }

    /// True when the card carries at least one [`Mark::Diff`] — the only
    /// content that earns graded disclosure (L1 header ↔ L3 full).  A card
    /// of only `text`/`fields`/`measure`/`raw` is chrome-level (L3-only).
    pub fn has_diff(&self) -> bool {
        self.0.iter().any(|m| matches!(m, Mark::Diff { .. }))
    }

    /// The card's magnitude — total changed lines summed across its `diff`
    /// marks, or `None` when it carries no diff.  The rail's value-step and
    /// the matrix's per-agent size readout both read this.
    pub fn magnitude(&self) -> Option<u32> {
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
    pub fn single_diff(&self) -> Option<(&str, &[Hunk])> {
        match self.0.as_slice() {
            [Mark::Diff { path, hunks }] => Some((path, hunks)),
            _ => None,
        }
    }

    /// Consume a single-`diff` card into its owned `(path, hunks)` for the
    /// patch-aggregation buffer; `Err(self)` hands a richer card back
    /// untouched so the caller can push it as its own block.
    pub fn into_single_diff(self) -> Result<(String, Vec<Hunk>), Self> {
        if self.single_diff().is_some() {
            let Card(mut marks) = self;
            match marks.pop() {
                Some(Mark::Diff { path, hunks }) => Ok((path, hunks)),
                _ => unreachable!("single_diff checked exactly one diff mark"),
            }
        } else {
            Err(self)
        }
    }
}

/// Total changed lines (deletions + additions) across `hunks` — the diff
/// magnitude, shared by [`Card::magnitude`] and the renderer's size-bar.
pub fn hunk_magnitude(hunks: &[Hunk]) -> u32 {
    hunks
        .iter()
        .map(|h| (h.del.len() + h.add.len()) as u32)
        .sum()
}

// ── Decode: runtime `Value` → `Card` ────────────────────────────────────────

/// Decode the value a ral kit handed to `surface` into a [`Card`].
///
/// The canonical shape is `` `card [mark, mark, …] `` — a variant whose
/// payload is a *list* of mark variants, each carrying a record payload.
/// A bare known mark surfaced unwrapped (`` `diff […] ``) is lifted into a
/// one-mark card for the model's convenience.  Anything else returns
/// `None` and is dropped, exactly as the old `value_to_kind` dropped an
/// unrecognised variant.
///
/// Decoding never fails *within* a recognised card: an unknown mark label
/// or role degrades to plain `text` rather than dropping the whole card,
/// because a card is a deliberate user-facing act, not a sentinel that
/// might be malformed.
pub fn value_to_card(v: &RalValue) -> Option<Card> {
    let RalValue::Variant { label, payload } = v else {
        return None;
    };
    if label == "card" {
        let marks = match payload.as_deref() {
            Some(RalValue::List(items)) => items.iter().map(decode_mark).collect(),
            // A `card` with a non-list payload is still a deliberate
            // surface; render whatever single mark it holds, or nothing.
            Some(other) => vec![decode_mark(other)],
            None => Vec::new(),
        };
        Some(Card(marks))
    } else if is_mark_label(label) {
        Some(Card(vec![decode_mark(v)]))
    } else {
        None
    }
}

/// The closed mark vocabulary, by tag — also the set lifted into a one-mark
/// card when surfaced unwrapped.
fn is_mark_label(label: &str) -> bool {
    matches!(label, "text" | "measure" | "fields" | "diff" | "raw")
}

/// Decode one mark.  Total: an unrecognised or malformed mark becomes a
/// plain `text` span carrying the value's display, never a drop or panic.
fn decode_mark(v: &RalValue) -> Mark {
    let RalValue::Variant { label, payload } = v else {
        return plain_text(&v.to_string());
    };
    let rec = match payload.as_deref() {
        Some(RalValue::Map(m)) => Some(m),
        _ => None,
    };
    match label.as_str() {
        "text" => Mark::Text {
            spans: rec.map(decode_spans).unwrap_or_default(),
        },
        "measure" => rec
            .and_then(decode_measure)
            .map(Mark::Measure)
            .unwrap_or_else(|| plain_text(label)),
        "fields" => Mark::Fields {
            rows: rec.map(decode_rows).unwrap_or_default(),
        },
        "diff" => rec
            .and_then(decode_diff)
            .unwrap_or_else(|| plain_text(label)),
        "raw" => Mark::Raw {
            bytes: rec.map(decode_raw_bytes).unwrap_or_default(),
        },
        _ => plain_text(&v.to_string()),
    }
}

/// A one-span plain-text mark — the degradation target for an unknown or
/// malformed mark.
fn plain_text(text: &str) -> Mark {
    Mark::Text {
        spans: vec![Span {
            role: None,
            text: text.to_string(),
        }],
    }
}

/// Decode the `spans` field of a `text` mark (or a `text`-valued field).
fn decode_spans(m: &ral_core::types::Map) -> Vec<Span> {
    match m.get("spans") {
        Some(RalValue::List(items)) => items.iter().map(decode_span).collect(),
        _ => Vec::new(),
    }
}

/// Decode one span record `[role?, text]`.  A bare string is a roleless
/// span; anything stranger falls back to the value's display.
fn decode_span(v: &RalValue) -> Span {
    match v {
        RalValue::Map(m) => Span {
            role: str_field(m, "role").as_deref().and_then(Role::parse),
            text: str_field(m, "text").unwrap_or_default(),
        },
        RalValue::String(s) => Span {
            role: None,
            text: s.clone(),
        },
        other => Span {
            role: None,
            text: other.to_string(),
        },
    }
}

/// Decode a `measure` record; `None` (→ plain-text fallback) when the
/// magnitude `value` is absent or not an integer.
fn decode_measure(m: &ral_core::types::Map) -> Option<Measure> {
    Some(Measure {
        label: str_field(m, "label").unwrap_or_default(),
        value: count_field(m, "value")?,
        max: count_field(m, "max"),
        unit: str_field(m, "unit"),
    })
}

/// Decode the `rows` field of a `fields` mark into aligned `(label, value)`
/// fields.
fn decode_rows(m: &ral_core::types::Map) -> Vec<Field> {
    match m.get("rows") {
        Some(RalValue::List(items)) => items.iter().map(decode_field).collect(),
        _ => Vec::new(),
    }
}

/// Decode one `[label: …, value: …]` row record.  Rows are records, not
/// positional pairs, because ral types a list homogeneously — a `String`
/// label and a variant value could not share one positional list.  The
/// value column nests marks: a bare string is roleless inline text, a
/// `text` mark its spans, a `measure` mark a nested measure; anything else
/// renders as its display.
fn decode_field(v: &RalValue) -> Field {
    let Some(m) = map_of(v) else {
        return Field {
            label: v.to_string(),
            value: FieldVal::Inline(Vec::new()),
        };
    };
    let label = str_field(m, "label").unwrap_or_default();
    let value = match m.get("value") {
        None => FieldVal::Inline(Vec::new()),
        Some(RalValue::Variant { label, payload }) if label == "text" => {
            let spans = match payload.as_deref() {
                Some(RalValue::Map(m)) => decode_spans(m),
                _ => Vec::new(),
            };
            FieldVal::Inline(spans)
        }
        Some(RalValue::Variant { label, payload }) if label == "measure" => {
            match payload.as_deref().and_then(map_of).and_then(decode_measure) {
                Some(measure) => FieldVal::Measure(measure),
                None => FieldVal::Inline(Vec::new()),
            }
        }
        Some(other) => FieldVal::Inline(vec![decode_span(other)]),
    };
    Field { label, value }
}

/// Decode a `diff` record.  Accepts either a `hunks` list of hunk records
/// or the flat single-hunk shape (`start`/`before`/`del`/`add`/`after` on
/// the record itself), the form `agent.ral`'s `edit` emits per change.
/// `None` (→ plain-text fallback) when there is no `path`.
fn decode_diff(m: &ral_core::types::Map) -> Option<Mark> {
    let path = str_field(m, "path")?;
    let hunks = match m.get("hunks") {
        Some(RalValue::List(items)) => items.iter().filter_map(map_of).map(decode_hunk).collect(),
        _ => vec![decode_hunk(m)],
    };
    Some(Mark::Diff { path, hunks })
}

/// Decode one hunk record; missing context lists default to empty and a
/// missing `start` defaults to line 1, so a partially-formed diff still
/// renders rather than dropping.
fn decode_hunk(m: &ral_core::types::Map) -> Hunk {
    Hunk {
        start: count_field(m, "start").unwrap_or(1),
        before: lines_field(m, "before"),
        del: lines_field(m, "del"),
        add: lines_field(m, "add"),
        after: lines_field(m, "after"),
    }
}

/// Decode a `raw` mark's `bytes`: a `Bytes` value verbatim, a string's
/// UTF-8, or a list of integers as bytes.
fn decode_raw_bytes(m: &ral_core::types::Map) -> Vec<u8> {
    match m.get("bytes") {
        Some(RalValue::Bytes(b)) => b.clone(),
        Some(RalValue::String(s)) => s.clone().into_bytes(),
        Some(RalValue::List(items)) => items
            .iter()
            .filter_map(|v| match v {
                RalValue::Int(n) => Some(*n as u8),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `&Value` → `&Map` when it is one.
fn map_of(v: &RalValue) -> Option<&ral_core::types::Map> {
    match v {
        RalValue::Map(m) => Some(m),
        _ => None,
    }
}

/// A string-typed field of a record.
fn str_field(m: &ral_core::types::Map, field: &str) -> Option<String> {
    match m.get(field) {
        Some(RalValue::String(s)) => Some(s.clone()),
        _ => None,
    }
}

/// An integer-typed field clamped into `u32` (negatives floor to 0).
fn count_field(m: &ral_core::types::Map, field: &str) -> Option<u32> {
    match m.get(field) {
        Some(RalValue::Int(n)) => Some((*n).clamp(0, u32::MAX as i64) as u32),
        _ => None,
    }
}

/// A list-of-strings field; non-string elements render as their display so
/// the row stays faithful, and a missing or non-list field is empty.
fn lines_field(m: &ral_core::types::Map, field: &str) -> Vec<String> {
    match m.get(field) {
        Some(RalValue::List(items)) => items
            .iter()
            .map(|v| match v {
                RalValue::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `` `card [marks…] `` runtime value the way the kit does.
    fn card_value(marks: Vec<RalValue>) -> RalValue {
        RalValue::Variant {
            label: "card".into(),
            payload: Some(Box::new(RalValue::list(marks))),
        }
    }
    fn mark(label: &str, fields: Vec<(&str, RalValue)>) -> RalValue {
        RalValue::Variant {
            label: label.into(),
            payload: Some(Box::new(RalValue::map(
                fields.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            ))),
        }
    }
    fn s(text: &str) -> RalValue {
        RalValue::String(text.into())
    }
    fn list(items: Vec<RalValue>) -> RalValue {
        RalValue::list(items)
    }

    /// A full card with one of every mark decodes structurally, in order.
    #[test]
    fn decodes_every_mark() {
        let v = card_value(vec![
            mark(
                "text",
                vec![(
                    "spans",
                    list(vec![RalValue::map(vec![
                        ("role".into(), s("strong")),
                        ("text".into(), s("edited ")),
                    ])]),
                )],
            ),
            mark(
                "diff",
                vec![
                    ("path", s("a.rs")),
                    ("start", RalValue::Int(7)),
                    ("del", list(vec![s("x")])),
                    ("add", list(vec![s("y")])),
                ],
            ),
            mark(
                "fields",
                vec![(
                    "rows",
                    list(vec![RalValue::map(vec![
                        ("label".into(), s("tests")),
                        ("value".into(), s("42 passed")),
                    ])]),
                )],
            ),
            mark(
                "measure",
                vec![
                    ("label", s("crates")),
                    ("value", RalValue::Int(7)),
                    ("max", RalValue::Int(12)),
                ],
            ),
            mark("raw", vec![("bytes", s("hi"))]),
        ]);
        let Card(marks) = value_to_card(&v).expect("a card decodes");
        assert_eq!(marks.len(), 5);
        assert!(matches!(&marks[0], Mark::Text { spans } if spans[0].role == Some(Role::Strong)));
        assert!(matches!(&marks[1], Mark::Diff { path, hunks }
            if path == "a.rs" && hunks[0].start == 7 && hunks[0].del == ["x"] && hunks[0].add == ["y"]));
        assert!(matches!(&marks[2], Mark::Fields { rows } if rows[0].label == "tests"));
        assert!(matches!(&marks[3], Mark::Measure(m) if m.value == 7 && m.max == Some(12)));
        assert!(matches!(&marks[4], Mark::Raw { bytes } if bytes == b"hi"));
    }

    /// A non-`card` variant is dropped (→ `None`); a *bare known mark* is
    /// lifted into a one-mark card for convenience.
    #[test]
    fn drops_non_card_but_lifts_bare_mark() {
        assert!(value_to_card(&RalValue::String("nope".into())).is_none());
        assert!(
            value_to_card(&RalValue::Variant {
                label: "bogus".into(),
                payload: Some(Box::new(RalValue::map(vec![]))),
            })
            .is_none(),
            "an unknown top-level variant is not a card"
        );
        let bare = mark("diff", vec![("path", s("a.rs")), ("start", RalValue::Int(1))]);
        let Card(marks) = value_to_card(&bare).expect("a bare diff lifts");
        assert_eq!(marks.len(), 1);
        assert!(matches!(&marks[0], Mark::Diff { .. }));
    }

    /// An unknown *mark* inside a card degrades to plain text, never a drop
    /// or a panic — the whole card still renders.
    #[test]
    fn unknown_mark_degrades_to_plain_text() {
        let v = card_value(vec![
            mark("text", vec![("spans", list(vec![]))]),
            mark("wormhole", vec![("x", RalValue::Int(1))]),
        ]);
        let Card(marks) = value_to_card(&v).expect("card decodes");
        assert_eq!(marks.len(), 2);
        assert!(matches!(&marks[1], Mark::Text { .. }), "unknown → text");
    }

    /// A card serialises to a structured mark tree — the `transcript.jsonl`
    /// record — with each mark internally tagged and a `raw` mark carrying
    /// its bytes.  Only `raw` is opaque, and honestly so.
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
                before: vec![],
                del: vec!["x".into()],
                add: vec!["y".into(), "z".into()],
                after: vec![],
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
