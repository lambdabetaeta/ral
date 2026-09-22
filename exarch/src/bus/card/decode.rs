//! A kit's `` `card `` value into the typed [`Card`] model.
//!
//! Decoding is total: an unknown mark or malformed field degrades to plain
//! text rather than dropping the card around it, since a card is a deliberate
//! user-facing act.  `decode_surface` in `shell_eval.rs` calls in here.

use ral_core::serial::FOValue;

use super::diff::{Hunk, Row, Seg};
use super::value::{count_field, items, record, shown, str_field};
use super::{Card, Field, FieldVal, Mark, Measure, Readout, Role, Span};

/// Decode the value a kit handed to `surface` into a [`Card`].
///
/// The shape is `` `card [mark, mark, …] ``; a known mark surfaced bare
/// (`` `diff […] ``) lifts into a one-mark card.  Anything else is `None`.
pub(crate) fn value_to_card(v: &FOValue) -> Option<Card> {
    let FOValue::Variant { label, payload } = v else {
        return None;
    };
    if label == "card" {
        let marks = match payload.as_deref() {
            Some(FOValue::List { items }) => items.iter().map(decode_mark).collect(),
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

/// The mark labels a bare surface lifts into a one-mark card: [`decode_mark`]'s
/// arms.
fn is_mark_label(label: &str) -> bool {
    matches!(label, "text" | "measure" | "fields" | "diff" | "raw")
}

/// Decode one mark; anything unrecognised or malformed becomes a plain-text
/// span of the value's display, never a drop or a panic.
fn decode_mark(v: &FOValue) -> Mark {
    let FOValue::Variant { label, payload } = v else {
        return plain_text(&shown(v));
    };
    let rec = payload.as_deref().and_then(record);
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
        _ => plain_text(&shown(v)),
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

fn decode_spans(m: &FOValue) -> Vec<Span> {
    items(m, "spans").iter().map(decode_span).collect()
}

fn decode_span(v: &FOValue) -> Span {
    match v {
        FOValue::Map { .. } => Span {
            role: str_field(v, "role").as_deref().and_then(Role::parse),
            text: str_field(v, "text").unwrap_or_default(),
        },
        FOValue::String { value } => Span {
            role: None,
            text: value.clone(),
        },
        other => Span {
            role: None,
            text: shown(other),
        },
    }
}

/// The magnitude `value` is the one field a readout cannot default.
fn decode_readout(m: &FOValue) -> Option<Readout> {
    Some(Readout {
        value: count_field(m, "value")?,
        max: count_field(m, "max"),
        unit: str_field(m, "unit"),
    })
}

fn decode_measure(m: &FOValue) -> Option<Measure> {
    Some(Measure {
        label: str_field(m, "label").unwrap_or_default(),
        readout: decode_readout(m)?,
    })
}

fn decode_rows(m: &FOValue) -> Vec<Field> {
    items(m, "rows").iter().map(decode_field).collect()
}

/// A row is a record, not a positional pair, because ral types a list
/// homogeneously: a `String` label and a variant value could not share one.
fn decode_field(v: &FOValue) -> Field {
    let Some(m) = record(v) else {
        return Field {
            label: shown(v),
            value: FieldVal::Inline(Vec::new()),
        };
    };
    let label = str_field(m, "label").unwrap_or_default();
    let value = match m.field("value") {
        None => FieldVal::Inline(Vec::new()),
        Some(FOValue::Variant { label, payload }) if label == "text" => FieldVal::Inline(
            payload
                .as_deref()
                .and_then(record)
                .map(decode_spans)
                .unwrap_or_default(),
        ),
        // A nested `measure` is read for its readout alone: the row's own label
        // names it, so a `label` written here is dropped like any other field
        // the decoder does not read.
        Some(FOValue::Variant { label, payload }) if label == "measure" => {
            match payload.as_deref().and_then(record).and_then(decode_readout) {
                Some(readout) => FieldVal::Readout(readout),
                None => FieldVal::Inline(Vec::new()),
            }
        }
        Some(other) => FieldVal::Inline(vec![decode_span(other)]),
    };
    Field { label, value }
}

/// Decode a kit-composed `diff` — by hand, the shape `whole_file_hunks` builds
/// for the host's own write cards.  Only `path` is required.
fn decode_diff(m: &FOValue) -> Option<Mark> {
    let path = str_field(m, "path")?;
    let hunks = items(m, "hunks")
        .iter()
        .filter_map(record)
        .map(decode_hunk)
        .collect();
    Some(Mark::Diff { path, hunks })
}

/// A missing `start` defaults to 1: hunk rows count from the original line 1.
fn decode_hunk(m: &FOValue) -> Hunk {
    Hunk {
        start: count_field(m, "start").unwrap_or(1),
        rows: items(m, "rows")
            .iter()
            .filter_map(record)
            .map(decode_row)
            .collect(),
    }
}

/// An unrecognised or missing `tag` degrades to context, the one row kind that
/// claims nothing changed.
fn decode_row(m: &FOValue) -> Row {
    let segs = items(m, "segs")
        .iter()
        .filter_map(record)
        .map(decode_seg)
        .collect();
    match str_field(m, "tag").as_deref() {
        Some("del") => Row::Del(segs),
        Some("add") => Row::Add(segs),
        _ => Row::Context(segs),
    }
}

fn decode_seg(m: &FOValue) -> Seg {
    Seg {
        emph: m.field("emph").and_then(FOValue::as_bool) == Some(true),
        text: str_field(m, "text").unwrap_or_default(),
    }
}

/// A kit has no byte literal, so a string or a list of integers reads as bytes.
fn decode_raw_bytes(m: &FOValue) -> Vec<u8> {
    match m.field("bytes") {
        Some(FOValue::Bytes { value }) => value.clone(),
        Some(FOValue::String { value }) => value.clone().into_bytes(),
        Some(FOValue::List { items }) => items
            .iter()
            .filter_map(|v| u8::try_from(v.as_int()?).ok())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{card_value, int, list, map_value, s, variant};
    use super::*;

    fn mark(label: &str, fields: Vec<(&str, FOValue)>) -> FOValue {
        variant(label, map_value(fields))
    }
    /// The record shape [`decode_row`] lifts back into a [`Row`].
    fn seg_row(tag: &str, text: &str) -> FOValue {
        map_value(vec![
            ("tag", s(tag)),
            ("segs", list(vec![map_value(vec![("text", s(text))])])),
        ])
    }

    #[test]
    fn decodes_every_mark() {
        let v = card_value(vec![
            mark(
                "text",
                vec![(
                    "spans",
                    list(vec![map_value(vec![
                        ("role", s("strong")),
                        ("text", s("edited ")),
                    ])]),
                )],
            ),
            mark(
                "diff",
                vec![
                    ("path", s("a.rs")),
                    (
                        "hunks",
                        list(vec![map_value(vec![
                            ("start", int(7)),
                            ("rows", list(vec![seg_row("del", "x"), seg_row("add", "y")])),
                        ])]),
                    ),
                ],
            ),
            mark(
                "fields",
                vec![(
                    "rows",
                    list(vec![map_value(vec![
                        ("label", s("tests")),
                        ("value", s("42 passed")),
                    ])]),
                )],
            ),
            mark(
                "measure",
                vec![("label", s("crates")), ("value", int(7)), ("max", int(12))],
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
        assert!(
            matches!(&marks[3], Mark::Measure(m) if m.readout.value == 7 && m.readout.max == Some(12))
        );
        assert!(matches!(&marks[4], Mark::Raw { bytes } if bytes == b"hi"));
    }

    #[test]
    fn drops_non_card_but_lifts_bare_mark() {
        assert!(value_to_card(&s("nope")).is_none());
        assert!(
            value_to_card(&variant("bogus", map_value(vec![]))).is_none(),
            "an unknown top-level variant is not a card"
        );
        let bare = mark("diff", vec![("path", s("a.rs")), ("start", int(1))]);
        let Card(marks) = value_to_card(&bare).expect("a bare diff lifts");
        assert_eq!(marks.len(), 1);
        assert!(matches!(&marks[0], Mark::Diff { .. }));
    }

    /// The sibling marks of an unknown one still render.
    #[test]
    fn unknown_mark_degrades_to_plain_text() {
        let v = card_value(vec![
            mark("text", vec![("spans", list(vec![]))]),
            mark("wormhole", vec![("x", int(1))]),
        ]);
        let Card(marks) = value_to_card(&v).expect("card decodes");
        assert_eq!(marks.len(), 2);
        assert!(matches!(&marks[1], Mark::Text { .. }), "unknown → text");
    }
}
