//! The typed [`Card`] model back into a ral value — the encoder inverse to
//! `decode` on its image: `` `value_to_card ∘ encode_card = id` `` on every
//! `Card` the decoder can produce. Where the decoder accepts sugar, this
//! chooses the one tagged spelling it also accepts, so a round trip
//! normalises rather than merely surviving.

use ral_core::serial::FOValue;

use super::diff::{Row, Seg};
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
                (
                    "hunks",
                    list(
                        hunks
                            .iter()
                            .map(|h| {
                                record(vec![
                                    ("start", count(h.start)),
                                    ("rows", list(h.rows.iter().map(encode_row).collect())),
                                ])
                            })
                            .collect(),
                    ),
                ),
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
mod tests {
    use super::super::decode::value_to_card;
    use super::super::diff::{Hunk, Row, Seg};
    use super::*;

    fn round_trip(card: &Card) -> Card {
        value_to_card(&encode_card(card)).expect("an encoded card decodes")
    }

    /// Every mark variant, each optional field present and then absent —
    /// mirrors `decode::tests::decodes_every_mark`.
    #[test]
    fn round_trips_every_mark_with_optionals() {
        let full = Card(vec![
            Mark::Text {
                spans: vec![Span::new(Role::Strong, "edited "), Span::plain("x")],
            },
            Mark::Measure(Measure {
                label: "crates".into(),
                readout: Readout {
                    value: 7,
                    max: Some(12),
                    unit: Some("kb".into()),
                },
            }),
            Mark::Fields {
                rows: vec![
                    Field {
                        label: "tests".into(),
                        value: FieldVal::Inline(vec![Span::plain("42 passed")]),
                    },
                    Field {
                        label: "cov".into(),
                        value: FieldVal::Readout(Readout {
                            value: 3,
                            max: None,
                            unit: None,
                        }),
                    },
                ],
            },
            Mark::Diff {
                path: "a.rs".into(),
                hunks: vec![Hunk {
                    start: 7,
                    rows: vec![
                        Row::Del(vec![Seg {
                            emph: true,
                            text: "x".into(),
                        }]),
                        Row::Add(vec![Seg::plain("y")]),
                        Row::Context(vec![Seg::plain("z")]),
                    ],
                }],
            },
            Mark::Raw {
                bytes: b"hi".to_vec(),
            },
        ]);
        let got = round_trip(&full);
        assert_eq!(got.marks().len(), full.marks().len());
        assert!(matches!(&got.marks()[0],
            Mark::Text { spans } if spans[0].role == Some(Role::Strong) && spans[1].role.is_none()));
        assert!(matches!(&got.marks()[1],
            Mark::Measure(m) if m.readout.value == 7 && m.readout.max == Some(12)
                && m.readout.unit.as_deref() == Some("kb")));
        assert!(matches!(&got.marks()[2], Mark::Fields { rows }
            if rows.len() == 2
                && matches!(&rows[0].value, FieldVal::Inline(spans) if spans[0].text == "42 passed")
                && matches!(&rows[1].value, FieldVal::Readout(r) if r.value == 3 && r.max.is_none())));
        assert!(matches!(&got.marks()[3], Mark::Diff { path, hunks }
            if path == "a.rs" && hunks[0].start == 7
                && matches!(hunks[0].rows.as_slice(), [Row::Del(_), Row::Add(_), Row::Context(_)])
                && matches!(hunks[0].rows[0].segs(), [Seg { emph: true, .. }])));
        assert!(matches!(&got.marks()[4], Mark::Raw { bytes } if bytes == b"hi"));

        let bare = Card(vec![
            Mark::Text {
                spans: vec![Span::plain("plain")],
            },
            Mark::Measure(Measure {
                label: "n".into(),
                readout: Readout {
                    value: 1,
                    max: None,
                    unit: None,
                },
            }),
            Mark::Fields { rows: vec![] },
            Mark::Diff {
                path: "b.rs".into(),
                hunks: vec![],
            },
            Mark::Raw { bytes: Vec::new() },
        ]);
        let got_bare = round_trip(&bare);
        assert!(matches!(&got_bare.marks()[0],
            Mark::Text { spans } if spans[0].role.is_none()));
        assert!(matches!(&got_bare.marks()[1],
            Mark::Measure(m) if m.readout.max.is_none() && m.readout.unit.is_none()));
        assert!(matches!(&got_bare.marks()[3], Mark::Diff { hunks, .. } if hunks.is_empty()));
        assert!(matches!(&got_bare.marks()[4], Mark::Raw { bytes } if bytes.is_empty()));
    }

    /// Sugar the decoder lifts — a bare string span, a `` `card `` whose
    /// payload is one mark rather than a list, and a bare mark with no
    /// `` `card `` wrapper at all — normalises on the first decode; encoding
    /// and decoding again must agree with it. Covers every sugar arm in
    /// `value_to_card`, not just the list-wrapped one.
    #[test]
    fn sugar_normalizes_through_a_round_trip() {
        let text_mark = tagged(
            "text",
            record(vec![("spans", list(vec![string("bare string span")]))]),
        );
        for sugared in [
            tagged("card", list(vec![text_mark.clone()])),
            tagged("card", text_mark.clone()),
            text_mark,
        ] {
            let first = value_to_card(&sugared).expect("sugar decodes");
            let second = round_trip(&first);
            assert_eq!(
                serde_json::to_value(&first).expect("first decode serialises"),
                serde_json::to_value(&second).expect("second decode serialises"),
            );
        }
    }
}
