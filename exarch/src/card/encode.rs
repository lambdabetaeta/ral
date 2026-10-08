//! The typed [`Card`] model back into a ral value — the encoder inverse to
//! `decode` on its image: `` `value_to_card ∘ encode_card = id` `` on every
//! `Card` the decoder can produce. Where the decoder accepts sugar, this
//! chooses the one tagged spelling it also accepts, so a round trip
//! normalises rather than merely surviving.

use ral_core::first_order::FOValue;

use super::diff::{Diff, Hunk, Row, Seg};
use super::{Card, Field, FieldVal, Mark, Measure, Readout, Role, Span};

fn string(value: impl Into<String>) -> FOValue {
    FOValue::String {
        value: value.into(),
    }
}

fn count(value: u32) -> FOValue {
    FOValue::Int {
        value: i64::from(value),
    }
}

fn list(items: Vec<FOValue>) -> FOValue {
    FOValue::List { items }
}

fn record(entries: Vec<(&str, FOValue)>) -> FOValue {
    FOValue::Map {
        entries: entries.into_iter().map(|(k, v)| (k.into(), v)).collect(),
    }
}

fn tagged(label: &str, payload: FOValue) -> FOValue {
    FOValue::Variant {
        label: label.into(),
        payload: Some(Box::new(payload)),
    }
}

/// Encode a decoded [`Card`] as the canonical `` `card [mark, …] `` value —
/// always the list form, even for a single mark, so a reader need not branch
/// on payload shape the way the decoder's sugar does.
pub(crate) fn encode_card(card: &Card) -> FOValue {
    tagged("card", list(card.marks().iter().map(encode_mark).collect()))
}

fn role_str(role: Role) -> &'static str {
    match role {
        Role::Path => "path",
        Role::Code => "code",
        Role::Ok => "ok",
        Role::Warn => "warn",
        Role::Bad => "bad",
        Role::Muted => "muted",
        Role::Strong => "strong",
    }
}

fn encode_span(span: &Span) -> FOValue {
    let mut fields = Vec::new();
    if let Some(role) = span.role {
        fields.push(("role", string(role_str(role))));
    }
    fields.push(("text", string(span.text.clone())));
    record(fields)
}

fn encode_spans(spans: &[Span]) -> FOValue {
    record(vec![(
        "spans",
        list(spans.iter().map(encode_span).collect()),
    )])
}

fn encode_readout_fields(readout: &Readout) -> Vec<(&'static str, FOValue)> {
    let mut fields = vec![("value", count(readout.value))];
    if let Some(max) = readout.max {
        fields.push(("max", count(max)));
    }
    if let Some(unit) = &readout.unit {
        fields.push(("unit", string(unit.clone())));
    }
    fields
}

fn encode_measure(measure: &Measure) -> FOValue {
    let mut fields = vec![("label", string(measure.label.clone()))];
    fields.extend(encode_readout_fields(&measure.readout));
    record(fields)
}

fn encode_field_val(val: &FieldVal) -> FOValue {
    match val {
        FieldVal::Inline(spans) => tagged("text", encode_spans(spans)),
        FieldVal::Readout(r) => tagged("measure", record(encode_readout_fields(r))),
    }
}

fn encode_field(field: &Field) -> FOValue {
    record(vec![
        ("label", string(field.label.clone())),
        ("value", encode_field_val(&field.value)),
    ])
}

fn encode_seg(seg: &Seg) -> FOValue {
    let mut fields = vec![("text", string(seg.text.clone()))];
    if seg.emph {
        fields.push(("emph", FOValue::Bool { value: true }));
    }
    record(fields)
}

fn encode_row(row: &Row) -> FOValue {
    let (tag, segs) = match row {
        Row::Context(segs) => ("context", segs),
        Row::Del(segs) => ("del", segs),
        Row::Add(segs) => ("add", segs),
    };
    record(vec![
        ("tag", string(tag)),
        ("segs", list(segs.iter().map(encode_seg).collect())),
    ])
}

fn encode_hunks(hunks: &[Hunk]) -> FOValue {
    list(
        hunks
            .iter()
            .map(|h| {
                record(vec![
                    ("old", count(h.old)),
                    ("new", count(h.new)),
                    ("rows", list(h.rows.iter().map(encode_row).collect())),
                ])
            })
            .collect(),
    )
}

/// What an edit surfaces: the file it changed and the diff it took there,
/// read back by [`value_to_edit`](super::decode::value_to_edit).
pub(crate) fn encode_edit(path: &str, diff: &Diff) -> FOValue {
    tagged(
        "edit",
        record(vec![
            ("path", string(path)),
            ("hunks", encode_hunks(&diff.hunks)),
            ("added", count(diff.added)),
            ("removed", count(diff.removed)),
        ]),
    )
}

fn encode_mark(mark: &Mark) -> FOValue {
    match mark {
        Mark::Text { spans } => tagged("text", encode_spans(spans)),
        Mark::Measure(m) => tagged("measure", encode_measure(m)),
        Mark::Fields { rows } => tagged(
            "fields",
            record(vec![(
                "rows",
                list(rows.iter().map(encode_field).collect()),
            )]),
        ),
        Mark::Diff { path, hunks } => tagged(
            "diff",
            record(vec![
                ("path", string(path.clone())),
                ("hunks", encode_hunks(hunks)),
            ]),
        ),
        Mark::Raw { bytes } => tagged(
            "raw",
            record(vec![(
                "bytes",
                FOValue::Bytes {
                    value: bytes.clone(),
                },
            )]),
        ),
    }
}

#[cfg(test)]
mod tests;
