//! The typed [`Card`] model back into a ral value — the encoder inverse to
//! `decode` on its image: `` `value_to_card ∘ encode_card = id` `` on every
//! `Card` the decoder can produce. Where the decoder accepts sugar, this
//! chooses the one tagged spelling it also accepts, so a round trip
//! normalises rather than merely surviving.

use ral_core::Value as RalValue;

use super::diff::{Row, Seg};
use super::{Card, Field, FieldVal, Mark, Measure, Role, Span};

/// Encode a decoded [`Card`] as the canonical `` `card [mark, …] `` value —
/// always the list form, even for a single mark, so a reader need not branch
/// on payload shape the way the decoder's sugar does.
pub(crate) fn encode_card(card: &Card) -> RalValue {
    RalValue::Variant {
        label: "card".into(),
        payload: Some(Box::new(RalValue::list(
            card.marks().iter().map(encode_mark).collect(),
        ))),
    }
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

fn encode_span(span: &Span) -> RalValue {
    let mut fields = Vec::new();
    if let Some(role) = span.role {
        fields.push(("role".to_string(), RalValue::String(role_str(role).into())));
    }
    fields.push(("text".to_string(), RalValue::String(span.text.clone())));
    RalValue::map(fields)
}

fn encode_spans(spans: &[Span]) -> RalValue {
    RalValue::map(vec![(
        "spans".into(),
        RalValue::list(spans.iter().map(encode_span).collect()),
    )])
}

fn encode_measure_fields(measure: &Measure) -> Vec<(String, RalValue)> {
    let mut fields = vec![
        ("label".to_string(), RalValue::String(measure.label.clone())),
        ("value".to_string(), RalValue::Int(i64::from(measure.value))),
    ];
    if let Some(max) = measure.max {
        fields.push(("max".to_string(), RalValue::Int(i64::from(max))));
    }
    if let Some(unit) = &measure.unit {
        fields.push(("unit".to_string(), RalValue::String(unit.clone())));
    }
    fields
}

fn encode_field_val(val: &FieldVal) -> RalValue {
    match val {
        FieldVal::Inline(spans) => RalValue::Variant {
            label: "text".into(),
            payload: Some(Box::new(encode_spans(spans))),
        },
        FieldVal::Measure(m) => RalValue::Variant {
            label: "measure".into(),
            payload: Some(Box::new(RalValue::map(encode_measure_fields(m)))),
        },
    }
}

fn encode_field(field: &Field) -> RalValue {
    RalValue::map(vec![
        ("label".into(), RalValue::String(field.label.clone())),
        ("value".into(), encode_field_val(&field.value)),
    ])
}

fn encode_seg(seg: &Seg) -> RalValue {
    let mut fields = vec![("text".to_string(), RalValue::String(seg.text.clone()))];
    if seg.emph {
        fields.push(("emph".to_string(), RalValue::Bool(true)));
    }
    RalValue::map(fields)
}

fn encode_row(row: &Row) -> RalValue {
    let (tag, segs) = match row {
        Row::Context(segs) => ("context", segs),
        Row::Del(segs) => ("del", segs),
        Row::Add(segs) => ("add", segs),
    };
    RalValue::map(vec![
        ("tag".into(), RalValue::String(tag.into())),
        (
            "segs".into(),
            RalValue::list(segs.iter().map(encode_seg).collect()),
        ),
    ])
}

fn encode_mark(mark: &Mark) -> RalValue {
    match mark {
        Mark::Text { spans } => RalValue::Variant {
            label: "text".into(),
            payload: Some(Box::new(encode_spans(spans))),
        },
        Mark::Measure(m) => RalValue::Variant {
            label: "measure".into(),
            payload: Some(Box::new(RalValue::map(encode_measure_fields(m)))),
        },
        Mark::Fields { rows } => RalValue::Variant {
            label: "fields".into(),
            payload: Some(Box::new(RalValue::map(vec![(
                "rows".into(),
                RalValue::list(rows.iter().map(encode_field).collect()),
            )]))),
        },
        Mark::Diff { path, hunks } => RalValue::Variant {
            label: "diff".into(),
            payload: Some(Box::new(RalValue::map(vec![
                ("path".into(), RalValue::String(path.clone())),
                (
                    "hunks".into(),
                    RalValue::list(
                        hunks
                            .iter()
                            .map(|h| {
                                RalValue::map(vec![
                                    ("start".into(), RalValue::Int(i64::from(h.start))),
                                    (
                                        "rows".into(),
                                        RalValue::list(h.rows.iter().map(encode_row).collect()),
                                    ),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]))),
        },
        Mark::Raw { bytes } => RalValue::Variant {
            label: "raw".into(),
            payload: Some(Box::new(RalValue::map(vec![(
                "bytes".into(),
                RalValue::Bytes(bytes.clone()),
            )]))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::decode::value_to_card;
    use super::super::diff::{Hunk, Row, Seg};
    use super::super::testkit::{card_value, list, s};
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
                value: 7,
                max: Some(12),
                unit: Some("kb".into()),
            }),
            Mark::Fields {
                rows: vec![
                    Field {
                        label: "tests".into(),
                        value: FieldVal::Inline(vec![Span::plain("42 passed")]),
                    },
                    Field {
                        label: "cov".into(),
                        value: FieldVal::Measure(Measure {
                            label: "cov".into(),
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
            Mark::Measure(m) if m.value == 7 && m.max == Some(12) && m.unit.as_deref() == Some("kb")));
        assert!(matches!(&got.marks()[2], Mark::Fields { rows }
            if rows.len() == 2
                && matches!(&rows[0].value, FieldVal::Inline(spans) if spans[0].text == "42 passed")
                && matches!(&rows[1].value, FieldVal::Measure(m) if m.value == 3 && m.max.is_none())));
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
                value: 1,
                max: None,
                unit: None,
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
            Mark::Measure(m) if m.max.is_none() && m.unit.is_none()));
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
        let text_mark = RalValue::Variant {
            label: "text".into(),
            payload: Some(Box::new(RalValue::map(vec![(
                "spans".into(),
                list(vec![s("bare string span")]),
            )]))),
        };
        for sugared in [
            card_value(vec![text_mark.clone()]),
            RalValue::Variant {
                label: "card".into(),
                payload: Some(Box::new(text_mark.clone())),
            },
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
