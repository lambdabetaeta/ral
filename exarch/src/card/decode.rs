//! A kit's `` `card `` value into the typed [`Card`] model.
//!
//! Decoding is total: an unknown mark or malformed field degrades to plain
//! text rather than dropping the card around it, since a card is a deliberate
//! user-facing act.  `decode_surface` in `shell_eval.rs` calls in here.

use ral_core::first_order::FOValue;
use ral_core::types::WriteOutcome;

use super::change::Change;
use super::diff::{Diff, Hunk, Row, Seg};
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

/// Read back what [`encode_edit`](super::encode::encode_edit) surfaced: an
/// edit is a change that committed, with the diff it took.
pub(crate) fn value_to_edit(v: &FOValue) -> Option<Change> {
    let FOValue::Variant { label, payload } = v else {
        return None;
    };
    if label != "edit" {
        return None;
    }
    let m = record(payload.as_deref()?)?;
    Some(Change {
        path: str_field(m, "path")?,
        outcome: WriteOutcome::Committed,
        diff: Some(Diff {
            hunks: items(m, "hunks")
                .iter()
                .filter_map(record)
                .map(decode_hunk)
                .collect(),
            added: count_field(m, "added")?,
            removed: count_field(m, "removed")?,
        }),
    })
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

/// Decode a kit-composed `diff` — by hand, the shape `Diff::between` builds
/// for the host's own changes.  Only `path` is required.
fn decode_diff(m: &FOValue) -> Option<Mark> {
    let path = str_field(m, "path")?;
    let hunks = items(m, "hunks")
        .iter()
        .filter_map(record)
        .map(decode_hunk)
        .collect();
    Some(Mark::Diff { path, hunks })
}

/// A missing `old` or `new` defaults to 1, the file's first line.
fn decode_hunk(m: &FOValue) -> Hunk {
    Hunk {
        old: count_field(m, "old").unwrap_or(1),
        new: count_field(m, "new").unwrap_or(1),
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
mod tests;
