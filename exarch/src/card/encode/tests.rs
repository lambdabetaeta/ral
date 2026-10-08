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
                old: 7,
                new: 9,
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
        if path == "a.rs" && hunks[0].old == 7 && hunks[0].new == 9
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
