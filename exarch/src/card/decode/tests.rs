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
                        ("old", int(7)),
                        ("new", int(9)),
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
        if path == "a.rs" && hunks[0].old == 7 && hunks[0].new == 9
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
    let bare = mark("diff", vec![("path", s("a.rs"))]);
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
