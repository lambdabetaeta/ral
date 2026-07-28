//! A kit's `` `card `` value into the typed [`Card`] model.
//!
//! Decoding is total: an unknown mark or malformed field degrades to plain
//! text rather than dropping the card around it, since a card is a deliberate
//! user-facing act.  `decode_surface` in `shell_eval.rs` calls in here.

use ral_core::Value as RalValue;

use super::diff::{Hunk, Row, Seg};
use super::value::{count_field, map_of, str_field};
use super::{Card, Field, FieldVal, Mark, Measure, Role, Span};

/// Decode the value a kit handed to `surface` into a [`Card`].
///
/// The shape is `` `card [mark, mark, …] ``; a known mark surfaced bare
/// (`` `diff […] ``) lifts into a one-mark card.  Anything else is `None`.
pub(crate) fn value_to_card(v: &RalValue) -> Option<Card> {
    let RalValue::Variant { label, payload } = v else {
        return None;
    };
    if label == "card" {
        let marks = match payload.as_deref() {
            Some(RalValue::List(items)) => items.iter().map(decode_mark).collect(),
            // A non-list payload is still a deliberate surface: its one mark.
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

/// Decode a `` `pin ``/`` `unpin `` wrapper into its register key and body card.
///
/// The wrapper carries placement only; an absent or empty body reads as
/// `` `unpin ``, so a pin with nothing left to show drops the slot.
pub(crate) fn value_to_pin(v: &RalValue) -> Option<(String, Option<Card>)> {
    let RalValue::Variant { label, payload } = v else {
        return None;
    };
    match label.as_str() {
        "pin" => {
            let m = map_of(payload.as_deref()?)?;
            let key = str_field(m, "key")?;
            let body = m
                .get("body")
                .and_then(value_to_card)
                .filter(|c| !c.marks().is_empty());
            Some((key, body))
        }
        "unpin" => Some((str_field(map_of(payload.as_deref()?)?, "key")?, None)),
        _ => None,
    }
}

/// The mark labels a bare surface lifts into a one-mark card: [`decode_mark`]'s
/// arms, less [`Mark::Listing`], which only the host composes.
fn is_mark_label(label: &str) -> bool {
    matches!(label, "text" | "measure" | "fields" | "diff" | "raw")
}

/// Decode one mark; anything unrecognised or malformed becomes a plain-text
/// span of the value's display, never a drop or a panic.
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
            .map_or_else(|| plain_text(label), Mark::Measure),
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

fn plain_text(text: &str) -> Mark {
    Mark::Text {
        spans: vec![Span {
            role: None,
            text: text.to_string(),
        }],
    }
}

fn decode_spans(m: &ral_core::types::Map) -> Vec<Span> {
    match m.get("spans") {
        Some(RalValue::List(items)) => items.iter().map(decode_span).collect(),
        _ => Vec::new(),
    }
}

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

/// The magnitude `value` is the one field a measure cannot default.
fn decode_measure(m: &ral_core::types::Map) -> Option<Measure> {
    Some(Measure {
        label: str_field(m, "label").unwrap_or_default(),
        value: count_field(m, "value")?,
        max: count_field(m, "max"),
        unit: str_field(m, "unit"),
    })
}

fn decode_rows(m: &ral_core::types::Map) -> Vec<Field> {
    match m.get("rows") {
        Some(RalValue::List(items)) => items.iter().map(decode_field).collect(),
        _ => Vec::new(),
    }
}

/// A row is a record, not a positional pair, because ral types a list
/// homogeneously: a `String` label and a variant value could not share one.
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

/// Decode a kit-composed `diff` — by hand, the shape `whole_file_hunks` builds
/// for the host's own write cards.  Only `path` is required.
fn decode_diff(m: &ral_core::types::Map) -> Option<Mark> {
    let path = str_field(m, "path")?;
    let hunks = match m.get("hunks") {
        Some(RalValue::List(items)) => items.iter().filter_map(map_of).map(decode_hunk).collect(),
        _ => Vec::new(),
    };
    Some(Mark::Diff { path, hunks })
}

/// A missing `start` defaults to 1: hunk rows count from the original line 1.
fn decode_hunk(m: &ral_core::types::Map) -> Hunk {
    let rows = match m.get("rows") {
        Some(RalValue::List(items)) => items.iter().filter_map(map_of).map(decode_row).collect(),
        _ => Vec::new(),
    };
    Hunk {
        start: count_field(m, "start").unwrap_or(1),
        rows,
    }
}

/// An unrecognised or missing `tag` degrades to context, the one row kind that
/// claims nothing changed.
fn decode_row(m: &ral_core::types::Map) -> Row {
    let segs = match m.get("segs") {
        Some(RalValue::List(items)) => items.iter().filter_map(map_of).map(decode_seg).collect(),
        _ => Vec::new(),
    };
    match str_field(m, "tag").as_deref() {
        Some("del") => Row::Del(segs),
        Some("add") => Row::Add(segs),
        _ => Row::Context(segs),
    }
}

fn decode_seg(m: &ral_core::types::Map) -> Seg {
    Seg {
        emph: matches!(m.get("emph"), Some(RalValue::Bool(true))),
        text: str_field(m, "text").unwrap_or_default(),
    }
}

/// A kit has no byte literal, so a string or a list of integers reads as bytes.
fn decode_raw_bytes(m: &ral_core::types::Map) -> Vec<u8> {
    match m.get("bytes") {
        Some(RalValue::Bytes(b)) => b.clone(),
        Some(RalValue::String(s)) => s.clone().into_bytes(),
        Some(RalValue::List(items)) => items
            .iter()
            .filter_map(|v| match v {
                RalValue::Int(n) => u8::try_from(*n).ok(),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{card_value, list, s};
    use super::*;

    fn mark(label: &str, fields: Vec<(&str, RalValue)>) -> RalValue {
        RalValue::Variant {
            label: label.into(),
            payload: Some(Box::new(RalValue::map(
                fields.into_iter().map(|(k, v)| (k.into(), v)).collect(),
            ))),
        }
    }
    /// The record shape [`decode_row`] lifts back into a [`Row`].
    fn seg_row(tag: &str, text: &str) -> RalValue {
        RalValue::map(vec![
            ("tag".into(), s(tag)),
            (
                "segs".into(),
                list(vec![RalValue::map(vec![("text".into(), s(text))])]),
            ),
        ])
    }

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
                    (
                        "hunks",
                        list(vec![RalValue::map(vec![
                            ("start".into(), RalValue::Int(7)),
                            (
                                "rows".into(),
                                list(vec![seg_row("del", "x"), seg_row("add", "y")]),
                            ),
                        ])]),
                    ),
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
            if path == "a.rs" && hunks[0].start == 7
                && matches!(hunks[0].rows.as_slice(), [Row::Del(_), Row::Add(_)])
                && hunks[0].rows.iter().map(Row::text).eq(["x", "y"].map(String::from))));
        assert!(matches!(&marks[2], Mark::Fields { rows } if rows[0].label == "tests"));
        assert!(matches!(&marks[3], Mark::Measure(m) if m.value == 7 && m.max == Some(12)));
        assert!(matches!(&marks[4], Mark::Raw { bytes } if bytes == b"hi"));
    }

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
        let bare = mark(
            "diff",
            vec![("path", s("a.rs")), ("start", RalValue::Int(1))],
        );
        let Card(marks) = value_to_card(&bare).expect("a bare diff lifts");
        assert_eq!(marks.len(), 1);
        assert!(matches!(&marks[0], Mark::Diff { .. }));
    }

    /// The sibling marks of an unknown one still render.
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
}
