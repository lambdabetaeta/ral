//! The one gate where a kit's ral value becomes a typed
//! [`crate::bus::card::Card`].
//!
//! Decoding is total: it degrades rather than raises.  An unknown mark
//! falls back to plain text, a malformed field to its default — so the
//! card always exists, and the renderer downstream never has to reckon
//! with one that isn't there.

use ral_core::Value as RalValue;

use super::diff::{Hunk, Row, Seg};
use super::value::{count_field, map_of, str_field};
use super::{Card, Field, FieldVal, Mark, Measure, Role, Span};

/// Decode the value a ral kit handed to `surface` into a [`crate::bus::card::Card`].
///
/// The canonical shape is `` `card [mark, mark, …] `` — a variant whose
/// payload is a *list* of mark variants, each carrying a record payload.
/// A bare known mark surfaced unwrapped (`` `diff […] ``) is lifted into a
/// one-mark card for the model's convenience.  Anything else returns
/// `None` and is dropped.
///
/// Decoding never fails *within* a recognised card: an unknown mark label
/// or role degrades to plain `text` rather than dropping the whole card,
/// because a card is a deliberate user-facing act, not a sentinel that
/// might be malformed.
pub(crate) fn value_to_card(v: &RalValue) -> Option<Card> {
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

/// Decode a `` `pin ``/`` `unpin `` *disposition wrapper* into its register key
/// and optional body card.
///
/// The shape is `` `pin [key: "…", body: `card […]] ``
/// — a render document keyed to a register slot — or `` `unpin [key: "…"] `` to
/// drop the slot.  The `body` is decoded by the **unchanged** [`value_to_card`],
/// so the wrapper carries only *placement*; an absent — or empty — body is the
/// same as `` `unpin ``, so a pin with nothing left to show drops the slot.
/// Anything else returns `None`, the same graceful degradation as
/// [`value_to_card`]; the decoder seam then drops it.
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

/// The mark tags a kit may surface — the decodable subset of
/// [`crate::bus::card::Mark`]; [`crate::bus::card::Mark::Listing`] is
/// host-composed only — also the set lifted into a one-mark card when
/// surfaced unwrapped.
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

/// Decode a `diff` record: its `path` and a `hunks` list of hunk records,
/// the whole-file shape `edit` emits.  A missing `hunks` lifts to an empty
/// vec so a bare diff still renders; `None` (→ plain-text fallback) only
/// when there is no `path`.
fn decode_diff(m: &ral_core::types::Map) -> Option<Mark> {
    let path = str_field(m, "path")?;
    let hunks = match m.get("hunks") {
        Some(RalValue::List(items)) => items.iter().filter_map(map_of).map(decode_hunk).collect(),
        _ => Vec::new(),
    };
    Some(Mark::Diff { path, hunks })
}

/// Decode one hunk record: a `start` line (defaulting to 1) and its `rows`
/// list of `{ tag, text }` records.  A missing `rows` defaults to empty, so
/// a partially-formed hunk still renders rather than dropping.
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

/// Decode one row record: its `tag` (`context` / `del` / `add`) and its
/// `segs` list.  An unrecognized or missing tag degrades to context — the row
/// is never dropped or panicked on, so the whole diff still renders.
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

/// Decode one segment record: its `emph` flag (defaulting to unemphasised)
/// and `text`.
fn decode_seg(m: &ral_core::types::Map) -> Seg {
    Seg {
        emph: matches!(m.get("emph"), Some(RalValue::Bool(true))),
        text: str_field(m, "text").unwrap_or_default(),
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
    /// A diff-row record: a `tag` and a one-segment `segs` list carrying
    /// `text` (unemphasised) — the shape [`decode_row`] lifts back into a
    /// [`crate::bus::card::Row`].
    fn seg_row(tag: &str, text: &str) -> RalValue {
        RalValue::map(vec![
            ("tag".into(), s(tag)),
            (
                "segs".into(),
                list(vec![RalValue::map(vec![("text".into(), s(text))])]),
            ),
        ])
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
        let bare = mark(
            "diff",
            vec![("path", s("a.rs")), ("start", RalValue::Int(1))],
        );
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
}
